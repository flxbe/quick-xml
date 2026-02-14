//! This is an implementation of [`Reader`] for reading from a `&[u8]` as
//! underlying byte stream. This implementation supports not using an
//! intermediate buffer as the byte slice itself can be used to borrow from.

use std::borrow::Cow;
use std::io;

use crate::parser::fast_element::FastElementParser;
#[cfg(feature = "encoding")]
use crate::reader::EncodingRef;
#[cfg(feature = "encoding")]
use encoding_rs::{Encoding, UTF_8};

use crate::errors::{Error, IllFormedError, Result, SyntaxError};
use crate::events::{BytesRef, Event};
use crate::name::QName;
use crate::parser::{Parser, PiParser};
use crate::reader::{BangType, ParseState, Reader, Span};
use crate::utils::is_whitespace;

/// This is an implementation for reading from a `&[u8]` as underlying byte stream.
/// This implementation supports not using an intermediate buffer as the byte slice
/// itself can be used to borrow from.
impl<'a> Reader<&'a [u8]> {
    /// Creates an XML reader from a string slice.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &'a str) -> Self {
        // Rust strings are guaranteed to be UTF-8, so lock the encoding
        #[cfg(feature = "encoding")]
        {
            let mut reader = Self::from_reader(s.as_bytes());
            reader.state.encoding = EncodingRef::Explicit(UTF_8);
            reader
        }

        #[cfg(not(feature = "encoding"))]
        Self::from_reader(s.as_bytes())
    }

    /// Read an event that borrows from the input rather than a buffer.
    ///
    /// There is no asynchronous `read_event_async()` version of this function,
    /// because it is not necessary -- the contents are already in memory and no IO
    /// is needed, therefore there is no potential for blocking.
    ///
    /// # Examples
    ///
    /// ```
    /// # use pretty_assertions::assert_eq;
    /// use quick_xml::events::Event;
    /// use quick_xml::reader::Reader;
    ///
    /// let mut reader = Reader::from_str(r#"
    ///     <tag1 att1 = "test">
    ///        <tag2><!--Test comment-->Test</tag2>
    ///        <tag2>Test 2</tag2>
    ///     </tag1>
    /// "#);
    /// reader.config_mut().trim_text(true);
    ///
    /// let mut count = 0;
    /// let mut txt = Vec::new();
    /// loop {
    ///     match reader.read_event().unwrap() {
    ///         Event::Start(e) => count += 1,
    ///         Event::Text(e) => txt.push(e.decode().unwrap().into_owned()),
    ///         Event::Eof => break,
    ///         _ => (),
    ///     }
    /// }
    /// assert_eq!(count, 3);
    /// assert_eq!(txt, vec!["Test".to_string(), "Test 2".to_string()]);
    /// ```
    #[inline]
    pub fn read_event(&mut self) -> Result<Event<'a>> {
        let event = match self.state.state {
            ParseState::Init => {
                // Go to InsideText state
                // If encoding set explicitly, we not need to detect it. For example,
                // explicit UTF-8 set automatically if Reader was created using `from_str`.
                // But we still need to remove BOM for consistency with no encoding
                // feature enabled path
                #[cfg(feature = "encoding")]
                if let Some(encoding) = reader.detect_encoding()? {
                    if self.state.encoding.can_be_refined() {
                        self.state.encoding = crate::reader::EncodingRef::BomDetected(encoding);
                    }
                }

                // Removes UTF-8 BOM if it is present
                #[cfg(not(feature = "encoding"))]
                let _ = remove_utf8_bom(&mut self.reader)?;

                self.state.state = ParseState::InsideText;

                // Return directly to enable tail call optimization.
                return self.read_event();
            }
            ParseState::InsideRef => self.read_ref_event(),
            ParseState::InsideText => self.read_text_event(),
            // Go to InsideText state in next two arms
            ParseState::InsideMarkup => self.read_until_close(),
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

    /// Read bytes up to the `>` and skip it. This method is expected to be called
    /// after seeing the `<` symbol and skipping it. Inspects the next (current)
    /// symbol and returns an appropriate [`Event`]:
    ///
    /// |Symbol |Event
    /// |-------|-------------------------------------
    /// |`!`    |[`Comment`], [`CData`] or [`DocType`]
    /// |`/`    |[`End`]
    /// |`?`    |[`PI`]
    /// |_other_|[`Start`] or [`Empty`]
    ///
    /// Moves parser to the `InsideText` state.
    ///
    /// [`Comment`]: Event::Comment
    /// [`CData`]: Event::CData
    /// [`DocType`]: Event::DocType
    /// [`End`]: Event::End
    /// [`PI`]: Event::PI
    /// [`Start`]: Event::Start
    /// [`Empty`]: Event::Empty
    fn read_until_close(&mut self) -> Result<Event<'a>> {
        self.state.state = ParseState::InsideText;

        let start = self.state.offset;
        match peek_one(&mut self.reader) {
            // `<!` - comment, CDATA or DOCTYPE declaration
            Ok(Some(b'!')) => match self.read_bang_element() {
                Ok((bang_type, bytes)) => self.state.emit_bang(bang_type, bytes),
                Err(e) => {
                    // We want to report error at `<`, but offset was increased,
                    // so return it back (-1 for `<`)
                    self.state.last_error_offset = start - 1;
                    Err(e)
                }
            },
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

                match self.read_element() {
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
                match self.read_with(PiParser(false)) {
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
            Ok(Some(_)) => match self.read_element() {
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

    /// Reads until end element is found. This function is supposed to be called
    /// after you already read a [`Start`] event.
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
    /// The `end` parameter should contain name of the end element _in the reader
    /// encoding_. It is good practice to always get that parameter using
    /// [`BytesStart::to_end()`] method.
    ///
    /// The correctness of the skipped events does not checked, if you disabled
    /// the [`check_end_names`] option.
    ///
    /// There is no asynchronous `read_to_end_async()` version of this function,
    /// because it is not necessary -- the contents are already in memory and no IO
    /// is needed, therefore there is no potential for blocking.
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
    ///
    /// let start = BytesStart::new("outer");
    /// let end   = start.to_end().into_owned();
    ///
    /// // First, we read a start event...
    /// assert_eq!(reader.read_event().unwrap(), Event::Start(start));
    ///
    /// // ...then, we could skip all events to the corresponding end event.
    /// // This call will correctly handle nested <outer> elements.
    /// // Note, however, that this method does not handle namespaces.
    /// reader.read_to_end(end.name()).unwrap();
    ///
    /// // At the end we should get an Eof event, because we ate the whole XML
    /// assert_eq!(reader.read_event().unwrap(), Event::Eof);
    /// ```
    ///
    /// [`Start`]: Event::Start
    /// [`End`]: Event::End
    /// [`BytesStart::to_end()`]: crate::events::BytesStart::to_end
    /// [`expand_empty_elements`]: crate::reader::Config::expand_empty_elements
    /// [`check_end_names`]: crate::reader::Config::check_end_names
    /// [the specification]: https://www.w3.org/TR/xml11/#dt-etag
    pub fn read_to_end(&mut self, end_name: QName) -> Result<Span> {
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
                let end = self.buffer_position();
                match self.read_event() {
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

    /// Reads content between start and end tags, including any markup. This
    /// function is supposed to be called after you already read a [`Start`] event.
    ///
    /// Manages nested cases where parent and child elements have the _literally_
    /// same name.
    ///
    /// This method does not unescape read data, instead it returns content
    /// "as is" of the XML document. This is because it has no idea what text
    /// it reads, and if, for example, it contains CDATA section, attempt to
    /// unescape it content will spoil data.
    ///
    /// Any text will be decoded using the XML current [`decoder()`].
    ///
    /// Actually, this method perform the following code:
    ///
    /// ```ignore
    /// let span = reader.read_to_end(end)?;
    /// let text = reader.decoder().decode(&reader.inner_slice[span]);
    /// ```
    ///
    /// # Examples
    ///
    /// This example shows, how you can read a HTML content from your XML document.
    ///
    /// ```
    /// # use pretty_assertions::assert_eq;
    /// # use std::borrow::Cow;
    /// use quick_xml::events::{BytesStart, Event};
    /// use quick_xml::reader::Reader;
    ///
    /// let mut reader = Reader::from_str("
    ///     <html>
    ///         <title>This is a HTML text</title>
    ///         <p>Usual XML rules does not apply inside it
    ///         <p>For example, elements not needed to be &quot;closed&quot;
    ///     </html>
    /// ");
    /// reader.config_mut().trim_text(true);
    ///
    /// let start = BytesStart::new("html");
    /// let end   = start.to_end().into_owned();
    ///
    /// // First, we read a start event...
    /// assert_eq!(reader.read_event().unwrap(), Event::Start(start));
    /// // ...and disable checking of end names because we expect HTML further...
    /// reader.config_mut().check_end_names = false;
    ///
    /// // ...then, we could read text content until close tag.
    /// // This call will correctly handle nested <html> elements.
    /// let text = reader.read_text(end.name()).unwrap();
    /// assert_eq!(text, Cow::Borrowed(r#"
    ///         <title>This is a HTML text</title>
    ///         <p>Usual XML rules does not apply inside it
    ///         <p>For example, elements not needed to be &quot;closed&quot;
    ///     "#));
    /// assert!(matches!(text, Cow::Borrowed(_)));
    ///
    /// // Now we can enable checks again
    /// reader.config_mut().check_end_names = true;
    ///
    /// // At the end we should get an Eof event, because we ate the whole XML
    /// assert_eq!(reader.read_event().unwrap(), Event::Eof);
    /// ```
    ///
    /// [`Start`]: Event::Start
    /// [`decoder()`]: Self::decoder()
    pub fn read_text(&mut self, end: QName) -> Result<Cow<'a, str>> {
        // self.reader will be changed, so store original reference
        let buffer = self.reader;
        let span = self.read_to_end(end)?;

        let len = span.end - span.start;
        // SAFETY: `span` can only contain indexes up to usize::MAX because it
        // was created from offsets from a single &[u8] slice
        Ok(self.decoder().decode(&buffer[0..len as usize])?)
    }

    #[inline]
    fn read_text_event(&mut self) -> Result<Event<'a>> {
        // Go to InsideMarkup or Done state
        if self.state.config.trim_text_start {
            skip_whitespace(&mut self.reader, &mut self.state.offset)?;
        }

        // Search for start of markup or an entity or character reference
        match memchr::memchr2(b'<', b'&', self.reader) {
            Some(0) if self.reader[0] == b'<' => {
                self.reader = &self.reader[1..];
                self.state.offset += 1;

                self.read_until_close()
            }
            // Do not consume `&` because it may be lone and we would be need to
            // return it as part of Text event
            Some(0) => self.read_ref_event(),
            Some(i) if self.reader[i] == b'<' => {
                let bytes = &self.reader[..i];
                self.reader = &self.reader[i + 1..];
                self.state.offset += i as u64 + 1;

                self.state.state = ParseState::InsideMarkup;
                // FIXME: Can produce an empty event if:
                // - event contains only spaces
                // - trim_text_start = false
                // - trim_text_end = true
                Ok(Event::Text(self.state.emit_text(bytes)))
            }
            Some(i) => {
                let (bytes, rest) = self.reader.split_at(i);
                self.reader = rest;
                self.state.offset += i as u64;

                self.state.state = ParseState::InsideRef;
                // Return Text event with `bytes` content or Eof if bytes is empty
                Ok(Event::Text(self.state.emit_text(bytes)))
            }
            None => {
                let bytes = &self.reader[..];
                self.reader = &[];
                self.state.offset += bytes.len() as u64;

                self.state.state = ParseState::Done;
                // Trim bytes from end if required
                let event = self.state.emit_text(bytes);
                if event.is_empty() {
                    Ok(Event::Eof)
                } else {
                    Ok(Event::Text(event))
                }
            }
        }
    }

    #[inline]
    fn read_ref_event(&mut self) -> Result<Event<'a>> {
        let start = self.state.offset;

        debug_assert_eq!(
            self.reader.first(),
            Some(&b'&'),
            "`read_ref` must be called at `&`"
        );
        // Search for the end of reference or a start of another reference or a markup
        match memchr::memchr3(b';', b'&', b'<', &self.reader[1..]) {
            // Do not consume `&` because it may be lone and we would be need to
            // return it as part of Text event
            Some(i) if self.reader[i + 1] == b'&' => {
                let (bytes, rest) = self.reader.split_at(i + 1);
                self.reader = rest;
                self.state.offset += i as u64 + 1;

                self.state.state = ParseState::InsideRef;

                // ReadRefResult::UpToRef(bytes)
                if self.state.config.allow_dangling_amp {
                    Ok(Event::Text(self.state.emit_text(bytes)))
                } else {
                    self.state.last_error_offset = start;
                    Err(Error::IllFormed(IllFormedError::UnclosedReference))
                }
            }
            Some(i) => {
                let end = i + 1;
                let is_end = self.reader[end] == b';';
                let bytes = &self.reader[..end];
                // +1 -- skip the end `;` or `<`
                self.reader = &self.reader[end + 1..];
                self.state.offset += end as u64 + 1;

                if is_end {
                    // ReadRefResult::Ref(bytes)
                    self.state.state = ParseState::InsideText;
                    // +1 to skip start `&`
                    Ok(Event::GeneralRef(BytesRef::wrap(
                        &bytes[1..],
                        self.decoder(),
                    )))
                } else {
                    // ReadRefResult::UpToMarkup(bytes)
                    if self.state.config.allow_dangling_amp {
                        self.state.state = ParseState::InsideMarkup;
                        Ok(Event::Text(self.state.emit_text(bytes)))
                    } else {
                        self.state.state = ParseState::InsideMarkup;
                        self.state.last_error_offset = start;
                        Err(Error::IllFormed(IllFormedError::UnclosedReference))
                    }
                }
            }
            None => {
                let bytes = &self.reader[..];
                self.reader = &[];
                self.state.offset += bytes.len() as u64;

                // ReadRefResult::UpToEof(bytes)
                if self.state.config.allow_dangling_amp {
                    self.state.state = ParseState::Done;
                    Ok(Event::Text(self.state.emit_text(bytes)))
                } else {
                    self.state.state = ParseState::Done;
                    self.state.last_error_offset = start;
                    Err(Error::IllFormed(IllFormedError::UnclosedReference))
                }
            }
        }
    }

    #[inline]
    fn read_with<P>(&mut self, mut parser: P) -> Result<&'a [u8]>
    where
        P: Parser,
    {
        if let Some(i) = parser.feed(self.reader) {
            // +1 for `>` which we do not include
            self.state.offset += i as u64 + 1;
            let bytes = &self.reader[..i];
            self.reader = &self.reader[i + 1..];
            return Ok(bytes);
        }

        self.state.offset += self.reader.len() as u64;
        Err(Error::Syntax(parser.eof_error(self.reader)))
    }

    #[inline]
    fn read_bang_element(&mut self) -> Result<(BangType, &'a [u8])> {
        // Peeked one bang ('!') before being called, so it's guaranteed to
        // start with it.
        debug_assert_eq!(self.reader[0], b'!');

        let mut bang_type = BangType::new(self.reader[1..].first().copied())?;

        if let Some((bytes, i)) = bang_type.parse(&[], self.reader) {
            self.state.offset += i as u64;
            self.reader = &self.reader[i..];
            return Ok((bang_type, bytes));
        }

        self.state.offset += self.reader.len() as u64;
        Err(bang_type.to_err().into())
    }

    #[inline]
    fn read_element(&mut self) -> Result<(usize, &'a [u8])> {
        let mut parser = FastElementParser::default();

        if let Some((name_len, consumed)) = parser.feed(self.reader) {
            // +1 for `>` which we do not include
            self.state.offset += consumed as u64 + 1;
            let bytes = &self.reader[..consumed];
            self.reader = &self.reader[consumed + 1..];
            return Ok((name_len, bytes));
        }

        self.state.offset += self.reader.len() as u64;
        Err(Error::Syntax(parser.eof_error(self.reader)))
    }
}

////////////////////////////////////////////////////////////////////////////////////////////////////

#[cfg(not(feature = "encoding"))]
#[inline]
fn remove_utf8_bom(source: &mut &[u8]) -> io::Result<()> {
    if source.starts_with(crate::encoding::UTF8_BOM) {
        *source = &source[crate::encoding::UTF8_BOM.len()..];
    }
    Ok(())
}

#[cfg(feature = "encoding")]
#[inline]
fn detect_encoding(source: &mut &[u8]) -> io::Result<Option<&'static Encoding>> {
    if let Some((enc, bom_len)) = crate::encoding::detect_encoding(source) {
        *source = &source[bom_len..];
        return Ok(Some(enc));
    }
    Ok(None)
}

#[inline]
fn skip_whitespace(source: &mut &[u8], position: &mut u64) -> io::Result<()> {
    let whitespaces = source
        .iter()
        .position(|b| !is_whitespace(*b))
        .unwrap_or(source.len());
    *position += whitespaces as u64;
    *source = &source[whitespaces..];
    Ok(())
}

#[inline]
fn peek_one(source: &mut &[u8]) -> io::Result<Option<u8>> {
    Ok(source.first().copied())
}

#[inline]
fn consume_one(source: &mut &[u8], position: &mut u64) -> io::Result<()> {
    *source = &source[1..];
    *position += 1;
    Ok(())
}
