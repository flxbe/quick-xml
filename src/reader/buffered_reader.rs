//! This is an implementation of [`Reader`] for reading from a [`BufRead`] as
//! underlying byte stream.

use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::path::Path;

use crate::errors::{Error, IllFormedError, Result, SyntaxError};
use crate::events::{BytesRef, Event};
use crate::name::QName;
use crate::parser::fast_element::FastElementParser;
use crate::parser::{Parser, PiParser};
use crate::reader::{BangType, ParseState, ReadRefResult, ReadTextResult, Reader, Span};
use crate::utils::is_whitespace;

#[cfg(not(feature = "encoding"))]
#[inline]
fn remove_utf8_bom<R: BufRead>(r: &mut R) -> io::Result<()> {
    use crate::encoding::UTF8_BOM;

    loop {
        break match r.fill_buf() {
            Ok(n) => {
                if n.starts_with(UTF8_BOM) {
                    r.consume(UTF8_BOM.len());
                }
                Ok(())
            }
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => Err(e),
        };
    }
}

#[cfg(feature = "encoding")]
#[inline]
fn detect_encoding<R: BufRead>(r: &mut R) -> io::Result<Option<&'static encoding_rs::Encoding>> {
    loop {
        break match r.fill_buf() {
            Ok(n) => {
                if let Some((enc, bom_len)) = crate::encoding::detect_encoding(n) {
                    r.consume(bom_len);
                    Ok(Some(enc))
                } else {
                    Ok(None)
                }
            }
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => Err(e),
        };
    }
}

#[inline]
fn read_text<'b, R: BufRead>(
    r: &mut R,
    buf: &'b mut Vec<u8>,
    position: &mut u64,
) -> ReadTextResult<'b, &'b mut Vec<u8>> {
    let mut read = 0;
    let start = buf.len();
    loop {
        let available = match r.fill_buf() {
            Ok(n) if n.is_empty() => break,
            Ok(n) => n,
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                *position += read;
                return ReadTextResult::Err(e);
            }
        };

        // Search for start of markup or an entity or character reference
        match memchr::memchr2(b'<', b'&', available) {
            // Special handling is needed only on the first iteration.
            // On next iterations we already read something and should emit Text event
            Some(0) if read == 0 && available[0] == b'<' => {
                r.consume(1);
                *position += 1;
                return ReadTextResult::Markup(buf);
            }
            // Do not consume `&` because it may be lone and we would be need to
            // return it as part of Text event
            Some(0) if read == 0 => return ReadTextResult::Ref(buf),
            Some(i) if available[i] == b'<' => {
                buf.extend_from_slice(&available[..i]);

                // +1 to skip `<`
                let used = i + 1;
                r.consume(used);
                read += used as u64;

                *position += read;
                return ReadTextResult::UpToMarkup(&buf[start..]);
            }
            Some(i) => {
                buf.extend_from_slice(&available[..i]);

                r.consume(i);
                read += i as u64;

                *position += read;
                return ReadTextResult::UpToRef(&buf[start..]);
            }
            None => {
                buf.extend_from_slice(available);

                let used = available.len();
                r.consume(used);
                read += used as u64;
            }
        }
    }

    *position += read;
    ReadTextResult::UpToEof(&buf[start..])
}

#[inline]
fn read_ref<'b, R: BufRead>(
    r: &mut R,
    buf: &'b mut Vec<u8>,
    position: &mut u64,
) -> ReadRefResult<'b> {
    let mut read = 0;
    let start = buf.len();
    loop {
        let available = match r.fill_buf() {
            Ok(n) if n.is_empty() => break,
            Ok(n) => n,
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                *position += read;
                return ReadRefResult::Err(e);
            }
        };
        // `read_ref` called when the first character is `&`, so we
        // should explicitly skip it at first iteration lest we confuse
        // it with the end
        if read == 0 {
            debug_assert_eq!(
                available.first(),
                Some(&b'&'),
                "`read_ref` must be called at `&`"
            );
            // If that ampersand is lone, then it will be part of text
            // and we should keep it
            buf.push(b'&');
            r.consume(1);
            read += 1;
            continue;
        }

        match memchr::memchr3(b';', b'&', b'<', available) {
            // Do not consume `&` because it may be lone and we would be need to
            // return it as part of Text event
            Some(i) if available[i] == b'&' => {
                buf.extend_from_slice(&available[..i]);

                r.consume(i);
                read += i as u64;

                *position += read;

                return ReadRefResult::UpToRef(&buf[start..]);
            }
            Some(i) => {
                let is_end = available[i] == b';';
                buf.extend_from_slice(&available[..i]);

                // +1 -- skip the end `;` or `<`
                let used = i + 1;
                r.consume(used);
                read += used as u64;

                *position += read;

                return if is_end {
                    ReadRefResult::Ref(&buf[start..])
                } else {
                    ReadRefResult::UpToMarkup(&buf[start..])
                };
            }
            None => {
                buf.extend_from_slice(available);

                let used = available.len();
                r.consume(used);
                read += used as u64;
            }
        }
    }

    *position += read;
    ReadRefResult::UpToEof(&buf[start..])
}

#[inline]
fn read_element<'b, R: BufRead>(
    r: &mut R,
    buf: &'b mut Vec<u8>,
    position: &mut u64,
) -> Result<(usize, &'b [u8])> {
    let mut parser = FastElementParser::default();
    let mut read = 0;
    let start = buf.len();
    loop {
        let available = match r.fill_buf() {
            Ok(n) if n.is_empty() => break,
            Ok(n) => n,
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                *position += read;
                return Err(Error::Io(e.into()));
            }
        };

        if let Some((name_len, consumed)) = parser.feed(available) {
            buf.extend_from_slice(&available[..consumed]);

            // +1 for `>` which we do not include
            r.consume(consumed + 1);
            read += consumed as u64 + 1;

            *position += read;
            return Ok((name_len, &buf[start..]));
        }

        // The `>` symbol not yet found, continue reading
        buf.extend_from_slice(available);

        let used = available.len();
        r.consume(used);
        read += used as u64;
    }

    *position += read;
    Err(Error::Syntax(parser.eof_error(&buf[start..])))
}

#[inline]
fn read_with<'b, R: BufRead, P: Parser>(
    r: &mut R,
    mut parser: P,
    buf: &'b mut Vec<u8>,
    position: &mut u64,
) -> Result<&'b [u8]> {
    let mut read = 0;
    let start = buf.len();
    loop {
        let available = match r.fill_buf() {
            Ok(n) if n.is_empty() => break,
            Ok(n) => n,
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                *position += read;
                return Err(Error::Io(e.into()));
            }
        };

        if let Some(i) = parser.feed(available) {
            buf.extend_from_slice(&available[..i]);

            // +1 for `>` which we do not include
            r.consume(i + 1);
            read += i as u64 + 1;

            *position += read;
            return Ok(&buf[start..]);
        }

        // The `>` symbol not yet found, continue reading
        buf.extend_from_slice(available);

        let used = available.len();
        r.consume(used);
        read += used as u64;
    }

    *position += read;
    Err(Error::Syntax(parser.eof_error(&buf[start..])))
}

#[inline]
fn read_bang_element<'b, R: BufRead>(
    r: &mut R,
    buf: &'b mut Vec<u8>,
    position: &mut u64,
) -> Result<(BangType, &'b [u8])> {
    // Peeked one bang ('!') before being called, so it's guaranteed to
    // start with it.
    let start = buf.len();
    let mut read = 1;
    buf.push(b'!');
    r.consume(1);

    let mut bang_type = BangType::new(peek_one(r)?)?;

    loop {
        match r.fill_buf() {
            // Note: Do not update position, so the error points to
            // somewhere sane rather than at the EOF
            Ok(n) if n.is_empty() => break,
            Ok(available) => {
                // We only parse from start because we don't want to consider
                // whatever is in the buffer before the bang element
                if let Some((consumed, used)) = bang_type.parse(&buf[start..], available) {
                    buf.extend_from_slice(consumed);

                    r.consume(used);
                    read += used as u64;

                    *position += read;
                    return Ok((bang_type, &buf[start..]));
                } else {
                    buf.extend_from_slice(available);

                    let used = available.len();
                    r.consume(used);
                    read += used as u64;
                }
            }
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                *position += read;
                return Err(Error::Io(e.into()));
            }
        }
    }

    *position += read;
    Err(bang_type.to_err().into())
}

#[inline]
fn skip_whitespace<R: BufRead>(r: &mut R, position: &mut u64) -> io::Result<()> {
    loop {
        break match r.fill_buf() {
            Ok(n) => {
                let count = n.iter().position(|b| !is_whitespace(*b)).unwrap_or(n.len());
                if count > 0 {
                    r.consume(count);
                    *position += count as u64;
                    continue;
                } else {
                    Ok(())
                }
            }
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => Err(e),
        };
    }
}

#[inline]
fn peek_one<R: BufRead>(r: &mut R) -> io::Result<Option<u8>> {
    loop {
        break match r.fill_buf() {
            Ok(n) => Ok(n.first().cloned()),
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => Err(e),
        };
    }
}

#[inline]
fn consume_one<R: BufRead>(r: &mut R, position: &mut u64) -> io::Result<()> {
    r.consume(1);
    *position += 1;
    Ok(())
}

////////////////////////////////////////////////////////////////////////////////////////////////////

/// This is an implementation for reading from a [`BufRead`] as underlying byte stream.
impl<R: BufRead> Reader<R> {
    /// Reads the next `Event`.
    ///
    /// This is the main entry point for reading XML `Event`s.
    ///
    /// `Event`s borrow `buf` and can be converted to own their data if needed (uses `Cow`
    /// internally).
    ///
    /// Having the possibility to control the internal buffers gives you some additional benefits
    /// such as:
    ///
    /// - Reduce the number of allocations by reusing the same buffer. For constrained systems,
    ///   you can call `buf.clear()` once you are done with processing the event (typically at the
    ///   end of your loop).
    /// - Reserve the buffer length if you know the file size (using `Vec::with_capacity`).
    ///
    /// # Examples
    ///
    /// ```
    /// # use pretty_assertions::assert_eq;
    /// use quick_xml::events::Event;
    /// use quick_xml::reader::Reader;
    ///
    /// let xml = r#"<tag1 att1 = "test">
    ///                 <tag2><!--Test comment-->Test</tag2>
    ///                 <tag2>Test 2</tag2>
    ///              </tag1>"#;
    /// let mut reader = Reader::from_str(xml);
    /// reader.config_mut().trim_text(true);
    /// let mut count = 0;
    /// let mut buf = Vec::new();
    /// let mut txt = Vec::new();
    /// loop {
    ///     match reader.read_event_into(&mut buf) {
    ///         Ok(Event::Start(_)) => count += 1,
    ///         Ok(Event::Text(e)) => txt.push(e.decode().unwrap().into_owned()),
    ///         Err(e) => panic!("Error at position {}: {:?}", reader.error_position(), e),
    ///         Ok(Event::Eof) => break,
    ///         _ => (),
    ///     }
    ///     buf.clear();
    /// }
    /// assert_eq!(count, 3);
    /// assert_eq!(txt, vec!["Test".to_string(), "Test 2".to_string()]);
    /// ```
    #[inline]
    pub fn read_event_into<'b>(&mut self, buf: &'b mut Vec<u8>) -> Result<Event<'b>> {
        let event = match self.state.state {
            ParseState::Init => {
                // Go to InsideText state
                // If encoding set explicitly, we not need to detect it. For example,
                // explicit UTF-8 set automatically if Reader was created using `from_str`.
                // But we still need to remove BOM for consistency with no encoding
                // feature enabled path
                #[cfg(feature = "encoding")]
                if let Some(encoding) = self.reader.detect_encoding()? {
                    if self.state.encoding.can_be_refined() {
                        self.state.encoding = crate::reader::EncodingRef::BomDetected(encoding);
                    }
                }

                // Removes UTF-8 BOM if it is present
                #[cfg(not(feature = "encoding"))]
                let _ = remove_utf8_bom(&mut self.reader)?;

                self.state.state = ParseState::InsideText;

                // Return directly to enable tail call optimization.
                return self.read_event_into(buf);
            }
            ParseState::InsideRef => {
                // Go to InsideText
                let start = self.state.offset;
                match read_ref(&mut self.reader, buf, &mut self.state.offset) {
                    // Emit reference, go to InsideText state
                    ReadRefResult::Ref(bytes) => {
                        self.state.state = ParseState::InsideText;
                        // +1 to skip start `&`
                        Ok(Event::GeneralRef(BytesRef::wrap(
                            &bytes[1..],
                            self.decoder(),
                        )))
                    }
                    // Go to Done state
                    ReadRefResult::UpToEof(bytes) if self.state.config.allow_dangling_amp => {
                        self.state.state = ParseState::Done;
                        Ok(Event::Text(self.state.emit_text(bytes)))
                    }
                    ReadRefResult::UpToEof(_) => {
                        self.state.state = ParseState::Done;
                        self.state.last_error_offset = start;
                        Err(Error::IllFormed(IllFormedError::UnclosedReference))
                    }
                    // Do not change state, stay in InsideRef
                    ReadRefResult::UpToRef(bytes) if self.state.config.allow_dangling_amp => {
                        Ok(Event::Text(self.state.emit_text(bytes)))
                    }
                    ReadRefResult::UpToRef(_) => {
                        self.state.last_error_offset = start;
                        Err(Error::IllFormed(IllFormedError::UnclosedReference))
                    }
                    // Go to InsideMarkup state
                    ReadRefResult::UpToMarkup(bytes) if self.state.config.allow_dangling_amp => {
                        self.state.state = ParseState::InsideMarkup;
                        Ok(Event::Text(self.state.emit_text(bytes)))
                    }
                    ReadRefResult::UpToMarkup(_) => {
                        self.state.state = ParseState::InsideMarkup;
                        self.state.last_error_offset = start;
                        Err(Error::IllFormed(IllFormedError::UnclosedReference))
                    }
                    ReadRefResult::Err(e) => Err(Error::Io(e.into())),
                }
            }
            ParseState::InsideText => {
                // Go to InsideMarkup or Done state
                if self.state.config.trim_text_start {
                    skip_whitespace(&mut self.reader, &mut self.state.offset)?;
                }

                match read_text(&mut self.reader, buf, &mut self.state.offset) {
                    ReadTextResult::Markup(buf) => self.read_until_close_impl(buf),
                    ReadTextResult::Ref(buf) => {
                        self.state.state = ParseState::InsideRef;
                        // Return immediately to allow for tail call optimization
                        return self.read_event_into(buf);
                    }
                    ReadTextResult::UpToMarkup(bytes) => {
                        self.state.state = ParseState::InsideMarkup;
                        // FIXME: Can produce an empty event if:
                        // - event contains only spaces
                        // - trim_text_start = false
                        // - trim_text_end = true
                        Ok(Event::Text(self.state.emit_text(bytes)))
                    }
                    ReadTextResult::UpToRef(bytes) => {
                        self.state.state = ParseState::InsideRef;
                        // Return Text event with `bytes` content or Eof if bytes is empty
                        Ok(Event::Text(self.state.emit_text(bytes)))
                    }
                    ReadTextResult::UpToEof(bytes) => {
                        self.state.state = ParseState::Done;
                        // Trim bytes from end if required
                        let event = self.state.emit_text(bytes);
                        if event.is_empty() {
                            Ok(Event::Eof)
                        } else {
                            Ok(Event::Text(event))
                        }
                    }
                    ReadTextResult::Err(e) => Err(Error::Io(e.into())),
                }
            }
            // Go to InsideText state in next two arms
            ParseState::InsideMarkup => self.read_until_close_impl(buf),
            ParseState::InsideEmpty => Ok(Event::End(self.state.close_expanded_empty())),
            ParseState::Done => Ok(Event::Eof),
        };

        match event {
            // #513: In case of ill-formed errors we already consume the wrong data
            // and change the state. We can continue parsing if we wish
            Err(Error::IllFormed(_)) => {}
            Err(_) | Ok(Event::Eof) => self.state.state = ParseState::Done,
            _ => {}
        }
        event
    }

    /// Private function to read until `>` is found. This function expects that
    /// it was called just after encounter a `<` symbol.
    fn read_until_close_impl<'b>(&mut self, buf: &'b mut Vec<u8>) -> Result<Event<'b>> {
        self.state.state = ParseState::InsideText;

        let start = self.state.offset;
        match peek_one(&mut self.reader) {
            // `<!` - comment, CDATA or DOCTYPE declaration
            Ok(Some(b'!')) => {
                match read_bang_element(&mut self.reader, buf, &mut self.state.offset) {
                    Ok((bang_type, bytes)) => self.state.emit_bang(bang_type, bytes),
                    Err(e) => {
                        // We want to report error at `<`, but offset was increased,
                        // so return it back (-1 for `<`)
                        self.state.last_error_offset = start - 1;
                        Err(e)
                    }
                }
            }
            // `</` - closing tag
            // #776: We parse using ElementParser which allows us to have attributes
            // in close tags. While such tags are not allowed by the specification,
            // we anyway allow to parse them because:
            // - we do not check constraints during parsing. This is performed by the
            //   optional validate step which user should call manually
            // - if we just look for `>` we will parse `</tag attr=">" >` as end tag
            //   `</tag attr=">` and text `" >` which probably no one existing parser
            //   does. This is malformed XML, however it is tolerated by some parsers
            //   (e.g. the one used by Adobe Flash) and such documents do exist in the wild.
            Ok(Some(b'/')) => {
                consume_one(&mut self.reader, &mut self.state.offset)?;

                match read_element(&mut self.reader, buf, &mut self.state.offset) {
                    Ok((name_len, bytes)) => self.state.emit_end(name_len, bytes),
                    Err(e) => {
                        // We want to report error at `<`, but offset was increased,
                        // so return it back (-1 for `<`)
                        self.state.last_error_offset = start - 1;
                        Err(e)
                    }
                }
            }
            // `<?` - processing instruction
            Ok(Some(b'?')) => {
                match read_with(
                    &mut self.reader,
                    PiParser(false),
                    buf,
                    &mut self.state.offset,
                ) {
                    Ok(bytes) => self.state.emit_question_mark(bytes),
                    Err(e) => {
                        // We want to report error at `<`, but offset was increased,
                        // so return it back (-1 for `<`)
                        self.state.last_error_offset = start - 1;
                        Err(e)
                    }
                }
            }
            // `<...` - opening or self-closed tag
            Ok(Some(_)) => match read_element(&mut self.reader, buf, &mut self.state.offset) {
                Ok((name_len, bytes)) => Ok(self.state.emit_start(name_len, bytes)),
                Err(e) => {
                    // We want to report error at `<`, but offset was increased,
                    // so return it back (-1 for `<`)
                    self.state.last_error_offset = start - 1;
                    Err(e)
                }
            },
            // `<` - syntax error, tag not closed
            Ok(None) => {
                // We want to report error at `<`, but offset was increased,
                // so return it back (-1 for `<`)
                self.state.last_error_offset = start - 1;
                Err(Error::Syntax(SyntaxError::UnclosedTag))
            }
            Err(e) => Err(Error::Io(e.into())),
        }
    }

    /// Reads until end element is found using provided buffer as intermediate
    /// storage for events content. This function is supposed to be called after
    /// you already read a [`Start`] event.
    ///
    /// Returns a span that cover content between `>` of an opening tag and `<` of
    /// a closing tag or an empty slice, if [`expand_empty_elements`] is set and
    /// this method was called after reading expanded [`Start`] event.
    ///
    /// Manages nested cases where parent and child elements have the _literally_
    /// same name.
    ///
    /// If a corresponding [`End`] event is not found, an error of type [`Error::IllFormed`]
    /// will be returned. In particularly, that error will be returned if you call
    /// this method without consuming the corresponding [`Start`] event first.
    ///
    /// If your reader created from a string slice or byte array slice, it is
    /// better to use [`read_to_end()`] method, because it will not copy bytes
    /// into intermediate buffer.
    ///
    /// The provided `buf` buffer will be filled only by one event content at time.
    /// Before reading of each event the buffer will be cleared. If you know an
    /// appropriate size of each event, you can preallocate the buffer to reduce
    /// number of reallocations.
    ///
    /// The `end` parameter should contain name of the end element _in the reader
    /// encoding_. It is good practice to always get that parameter using
    /// [`BytesStart::to_end()`] method.
    ///
    /// The correctness of the skipped events does not checked, if you disabled
    /// the [`check_end_names`] option.
    ///
    /// # Namespaces
    ///
    /// While the `Reader` does not support namespace resolution, namespaces
    /// does not change the algorithm for comparing names. Although the names
    /// `a:name` and `b:name` where both prefixes `a` and `b` resolves to the
    /// same namespace, are semantically equivalent, `</b:name>` cannot close
    /// `<a:name>`, because according to [the specification]
    ///
    /// > The end of every element that begins with a **start-tag** MUST be marked
    /// > by an **end-tag** containing a name that echoes the element's type as
    /// > given in the **start-tag**
    ///
    /// # Examples
    ///
    /// This example shows, how you can skip XML content after you read the
    /// start event.
    ///
    /// ```
    /// # use pretty_assertions::assert_eq;
    /// use quick_xml::events::{BytesStart, Event};
    /// use quick_xml::reader::Reader;
    ///
    /// let mut reader = Reader::from_str(r#"
    ///     <outer>
    ///         <inner>
    ///             <inner></inner>
    ///             <inner/>
    ///             <outer></outer>
    ///             <outer/>
    ///         </inner>
    ///     </outer>
    /// "#);
    /// reader.config_mut().trim_text(true);
    /// let mut buf = Vec::new();
    ///
    /// let start = BytesStart::new("outer");
    /// let end   = start.to_end().into_owned();
    ///
    /// // First, we read a start event...
    /// assert_eq!(reader.read_event_into(&mut buf).unwrap(), Event::Start(start));
    ///
    /// // ...then, we could skip all events to the corresponding end event.
    /// // This call will correctly handle nested <outer> elements.
    /// // Note, however, that this method does not handle namespaces.
    /// reader.read_to_end_into(end.name(), &mut buf).unwrap();
    ///
    /// // At the end we should get an Eof event, because we ate the whole XML
    /// assert_eq!(reader.read_event_into(&mut buf).unwrap(), Event::Eof);
    /// ```
    ///
    /// [`Start`]: Event::Start
    /// [`End`]: Event::End
    /// [`BytesStart::to_end()`]: crate::events::BytesStart::to_end
    /// [`read_to_end()`]: Self::read_to_end
    /// [`expand_empty_elements`]: crate::reader::Config::expand_empty_elements
    /// [`check_end_names`]: crate::reader::Config::check_end_names
    /// [the specification]: https://www.w3.org/TR/xml11/#dt-etag
    pub fn read_to_end_into(&mut self, end_name: QName, buf: &mut Vec<u8>) -> Result<Span> {
        Ok({
            // Because we take position after the event before the End event,
            // it is important that this position indicates beginning of the End event.
            // If between last event and the End event would be only spaces, then we
            // take position before the spaces, but spaces would be skipped without
            // generating event if `trim_text_start` is set to `true`. To prevent that
            // we temporary disable start text trimming.
            //
            // We also cannot take position after getting End event, because if
            // `trim_markup_names_in_closing_tags` is set to `true` (which is the default),
            // we do not known the real size of the End event that it is occupies in
            // the source and cannot correct the position after the End event.
            // So, we in any case should tweak parser configuration.
            let config = self.config_mut();
            let trim = config.trim_text_start;
            config.trim_text_start = false;

            let start = self.buffer_position();
            let mut depth = 0;
            loop {
                buf.clear();
                let end = self.buffer_position();
                match self.read_event_into(buf) {
                    Err(e) => {
                        self.config_mut().trim_text_start = trim;
                        return Err(e);
                    }

                    Ok(Event::Start(e)) if e.name() == end_name => depth += 1,
                    Ok(Event::End(e)) if e.name() == end_name => {
                        if depth == 0 {
                            self.config_mut().trim_text_start = trim;
                            break start..end;
                        }
                        depth -= 1;
                    }
                    Ok(Event::Eof) => {
                        self.config_mut().trim_text_start = trim;
                        return Err(Error::missed_end(end_name, self.decoder()));
                    }
                    _ => (),
                }
            }
        })
    }
}

impl Reader<BufReader<File>> {
    /// Creates an XML reader from a file path.
    pub fn from_file<P: AsRef<Path>>(path: P) -> Result<Self> {
        let file = File::open(path)?;
        let reader = BufReader::new(file);
        Ok(Self::from_reader(reader))
    }
}
