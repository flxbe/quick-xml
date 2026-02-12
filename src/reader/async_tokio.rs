//! This is an implementation of [`Reader`] for reading from a [`AsyncBufRead`]
//! as underlying byte stream. This reader fully implements async/await so reading
//! can use non-blocking I/O.

use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{self, AsyncBufRead, AsyncBufReadExt, AsyncRead, ReadBuf};

use crate::errors::{Error, IllFormedError, Result, SyntaxError};
use crate::events::{BytesRef, Event};
use crate::name::{QName, ResolveResult};
use crate::parser::{ElementParser, Parser, PiParser};
use crate::reader::{
    BangType, BinaryStream, NsReader, ParseState, ReadRefResult, ReadTextResult, Reader, Span,
};
use crate::utils::is_whitespace;

macro_rules! impl_buffered_source {
    ($($lf:lifetime, $reader:tt, $async:ident, $await:ident)?) => {
        #[cfg(not(feature = "encoding"))]
        #[inline]
        $($async)? fn remove_utf8_bom(&mut self) -> io::Result<()> {
            use crate::encoding::UTF8_BOM;

            loop {
                break match self $(.$reader)? .fill_buf() $(.$await)? {
                    Ok(n) => {
                        if n.starts_with(UTF8_BOM) {
                            self $(.$reader)? .consume(UTF8_BOM.len());
                        }
                        Ok(())
                    },
                    Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => Err(e),
                };
            }
        }

        #[cfg(feature = "encoding")]
        #[inline]
        $($async)? fn detect_encoding(&mut self) -> io::Result<Option<&'static encoding_rs::Encoding>> {
            loop {
                break match self $(.$reader)? .fill_buf() $(.$await)? {
                    Ok(n) => if let Some((enc, bom_len)) = crate::encoding::detect_encoding(n) {
                        self $(.$reader)? .consume(bom_len);
                        Ok(Some(enc))
                    } else {
                        Ok(None)
                    },
                    Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => Err(e),
                };
            }
        }

        #[inline]
        $($async)? fn read_text $(<$lf>)? (
            &mut self,
            buf: &'b mut Vec<u8>,
            position: &mut u64,
        ) -> ReadTextResult<'b, &'b mut Vec<u8>> {
            let mut read = 0;
            let start = buf.len();
            loop {
                let available = match self $(.$reader)? .fill_buf() $(.$await)? {
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
                        self $(.$reader)? .consume(1);
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
                        self $(.$reader)? .consume(used);
                        read += used as u64;

                        *position += read;
                        return ReadTextResult::UpToMarkup(&buf[start..]);
                    }
                    Some(i) => {
                        buf.extend_from_slice(&available[..i]);

                        self $(.$reader)? .consume(i);
                        read += i as u64;

                        *position += read;
                        return ReadTextResult::UpToRef(&buf[start..]);
                    }
                    None => {
                        buf.extend_from_slice(available);

                        let used = available.len();
                        self $(.$reader)? .consume(used);
                        read += used as u64;
                    }
                }
            }

            *position += read;
            ReadTextResult::UpToEof(&buf[start..])
        }

        #[inline]
        $($async)? fn read_ref $(<$lf>)? (
            &mut self,
            buf: &'b mut Vec<u8>,
            position: &mut u64,
        ) -> ReadRefResult<'b> {
            let mut read = 0;
            let start = buf.len();
            loop {
                let available = match self $(.$reader)? .fill_buf() $(.$await)? {
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
                    self $(.$reader)? .consume(1);
                    read += 1;
                    continue;
                }

                match memchr::memchr3(b';', b'&', b'<', available) {
                    // Do not consume `&` because it may be lone and we would be need to
                    // return it as part of Text event
                    Some(i) if available[i] == b'&' => {
                        buf.extend_from_slice(&available[..i]);

                        self $(.$reader)? .consume(i);
                        read += i as u64;

                        *position += read;

                        return ReadRefResult::UpToRef(&buf[start..]);
                    }
                    Some(i) => {
                        let is_end = available[i] == b';';
                        buf.extend_from_slice(&available[..i]);

                        // +1 -- skip the end `;` or `<`
                        let used = i + 1;
                        self $(.$reader)? .consume(used);
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
                        self $(.$reader)? .consume(used);
                        read += used as u64;
                    }
                }
            }

            *position += read;
            ReadRefResult::UpToEof(&buf[start..])
        }

        #[inline]
        $($async)? fn read_element<$($lf)?>(
            &mut self,
            buf: &'b mut Vec<u8>,
            position: &mut u64,
        ) -> Result<(usize, &'b [u8])> {
            let mut parser = FastElementParser::default();
            let mut read = 0;
            let start = buf.len();
            loop {
                let available = match self $(.$reader)? .fill_buf() $(.$await)? {
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
                    self $(.$reader)? .consume(consumed + 1);
                    read += consumed as u64 + 1;

                    *position += read;
                    return Ok((name_len, &buf[start..]));
                }

                // The `>` symbol not yet found, continue reading
                buf.extend_from_slice(available);

                let used = available.len();
                self $(.$reader)? .consume(used);
                read += used as u64;
            }

            *position += read;
            Err(Error::Syntax(parser.eof_error(&buf[start..])))
        }

        #[inline]
        $($async)? fn read_with<$($lf,)? P: Parser>(
            &mut self,
            mut parser: P,
            buf: &'b mut Vec<u8>,
            position: &mut u64,
        ) -> Result<&'b [u8]> {
            let mut read = 0;
            let start = buf.len();
            loop {
                let available = match self $(.$reader)? .fill_buf() $(.$await)? {
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
                    self $(.$reader)? .consume(i + 1);
                    read += i as u64 + 1;

                    *position += read;
                    return Ok(&buf[start..]);
                }

                // The `>` symbol not yet found, continue reading
                buf.extend_from_slice(available);

                let used = available.len();
                self $(.$reader)? .consume(used);
                read += used as u64;
            }

            *position += read;
            Err(Error::Syntax(parser.eof_error(&buf[start..])))
        }

        #[inline]
        $($async)? fn read_bang_element $(<$lf>)? (
            &mut self,
            buf: &'b mut Vec<u8>,
            position: &mut u64,
        ) -> Result<(BangType, &'b [u8])> {
            // Peeked one bang ('!') before being called, so it's guaranteed to
            // start with it.
            let start = buf.len();
            let mut read = 1;
            buf.push(b'!');
            self $(.$reader)? .consume(1);

            let mut bang_type = BangType::new(self.peek_one() $(.$await)? ?)?;

            loop {
                match self $(.$reader)? .fill_buf() $(.$await)? {
                    // Note: Do not update position, so the error points to
                    // somewhere sane rather than at the EOF
                    Ok(n) if n.is_empty() => break,
                    Ok(available) => {
                        // We only parse from start because we don't want to consider
                        // whatever is in the buffer before the bang element
                        if let Some((consumed, used)) = bang_type.parse(&buf[start..], available) {
                            buf.extend_from_slice(consumed);

                            self $(.$reader)? .consume(used);
                            read += used as u64;

                            *position += read;
                            return Ok((bang_type, &buf[start..]));
                        } else {
                            buf.extend_from_slice(available);

                            let used = available.len();
                            self $(.$reader)? .consume(used);
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
        $($async)? fn skip_whitespace(&mut self, position: &mut u64) -> io::Result<()> {
            loop {
                break match self $(.$reader)? .fill_buf() $(.$await)? {
                    Ok(n) => {
                        let count = n.iter().position(|b| !is_whitespace(*b)).unwrap_or(n.len());
                        if count > 0 {
                            self $(.$reader)? .consume(count);
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
        $($async)? fn peek_one(&mut self) -> io::Result<Option<u8>> {
            loop {
                break match self $(.$reader)? .fill_buf() $(.$await)? {
                    Ok(n) => Ok(n.first().cloned()),
                    Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => Err(e),
                };
            }
        }

        #[inline]
        $($async)? fn consume_one(&mut self, position: &mut u64) -> io::Result<()> {
            self $(.$reader)? .consume(1);
            *position += 1;
            Ok(())
        }
    };
}

/// A struct for read XML asynchronously from an [`AsyncBufRead`].
///
/// Having own struct allows us to implement anything without risk of name conflicts
/// and does not suffer from the impossibility of having `async` in traits.
struct TokioAdapter<'a, R>(&'a mut R);

impl<'a, R: AsyncBufRead + Unpin> TokioAdapter<'a, R> {
    impl_buffered_source!('b, 0, async, await);
}

////////////////////////////////////////////////////////////////////////////////////////////////////

impl<'r, R> AsyncRead for BinaryStream<'r, R>
where
    R: AsyncRead + Unpin,
{
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let start = buf.remaining();
        let this = self.get_mut();
        let poll = Pin::new(&mut *this.inner).poll_read(cx, buf);

        // If something was read, update offset
        if let Poll::Ready(Ok(_)) = poll {
            let amt = start - buf.remaining();
            *this.offset += amt as u64;
        }
        poll
    }
}

impl<'r, R> AsyncBufRead for BinaryStream<'r, R>
where
    R: AsyncBufRead + Unpin,
{
    #[inline]
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
        Pin::new(&mut *self.get_mut().inner).poll_fill_buf(cx)
    }

    #[inline]
    fn consume(self: Pin<&mut Self>, amt: usize) {
        let this = self.get_mut();
        this.inner.consume(amt);
        *this.offset += amt as u64;
    }
}

////////////////////////////////////////////////////////////////////////////////////////////////////

impl<R: AsyncBufRead + Unpin> Reader<R> {
    /// An asynchronous version of [`read_event_into()`]. Reads the next event into
    /// given buffer.
    ///
    /// This is the main entry point for reading XML `Event`s when using an async reader.
    ///
    /// See the documentation of [`read_event_into()`] for more information.
    ///
    /// # Examples
    ///
    /// ```
    /// # tokio_test::block_on(async {
    /// # use pretty_assertions::assert_eq;
    /// use quick_xml::events::Event;
    /// use quick_xml::reader::Reader;
    ///
    /// // This explicitly uses `from_reader("...".as_bytes())` to use a buffered
    /// // reader instead of relying on the zero-copy optimizations for reading
    /// // from byte slices, which provides the sync interface anyway.
    /// let mut reader = Reader::from_reader(r#"
    ///     <tag1 att1 = "test">
    ///        <tag2><!--Test comment-->Test</tag2>
    ///        <tag2>Test 2</tag2>
    ///     </tag1>
    /// "#.as_bytes());
    /// reader.config_mut().trim_text(true);
    ///
    /// let mut count = 0;
    /// let mut buf = Vec::new();
    /// let mut txt = Vec::new();
    /// loop {
    ///     match reader.read_event_into_async(&mut buf).await {
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
    /// # }) // tokio_test::block_on
    /// ```
    ///
    /// [`read_event_into()`]: Reader::read_event_into
    pub async fn read_event_into_async<'b>(
        &mut self,
        mut buf: &'b mut Vec<u8>,
    ) -> Result<Event<'b>> {
        read_event_impl!(
            self,
            buf,
            TokioAdapter(&mut self.reader),
            read_until_close_async,
            await
        )
    }

    /// An asynchronous version of [`read_to_end_into()`].
    /// Reads asynchronously until end element is found using provided buffer as
    /// intermediate storage for events content. This function is supposed to be
    /// called after you already read a [`Start`] event.
    ///
    /// See the documentation of [`read_to_end_into()`] for more information.
    ///
    /// # Examples
    ///
    /// This example shows, how you can skip XML content after you read the
    /// start event.
    ///
    /// ```
    /// # tokio_test::block_on(async {
    /// # use pretty_assertions::assert_eq;
    /// use quick_xml::events::{BytesStart, Event};
    /// use quick_xml::reader::Reader;
    ///
    /// let mut reader = Reader::from_reader(r#"
    ///     <outer>
    ///         <inner>
    ///             <inner></inner>
    ///             <inner/>
    ///             <outer></outer>
    ///             <outer/>
    ///         </inner>
    ///     </outer>
    /// "#.as_bytes());
    /// reader.config_mut().trim_text(true);
    /// let mut buf = Vec::new();
    ///
    /// let start = BytesStart::new("outer");
    /// let end   = start.to_end().into_owned();
    ///
    /// // First, we read a start event...
    /// assert_eq!(reader.read_event_into_async(&mut buf).await.unwrap(), Event::Start(start));
    ///
    /// // ...then, we could skip all events to the corresponding end event.
    /// // This call will correctly handle nested <outer> elements.
    /// // Note, however, that this method does not handle namespaces.
    /// reader.read_to_end_into_async(end.name(), &mut buf).await.unwrap();
    ///
    /// // At the end we should get an Eof event, because we ate the whole XML
    /// assert_eq!(reader.read_event_into_async(&mut buf).await.unwrap(), Event::Eof);
    /// # }) // tokio_test::block_on
    /// ```
    ///
    /// [`read_to_end_into()`]: Self::read_to_end_into
    /// [`Start`]: Event::Start
    pub async fn read_to_end_into_async<'n>(
        &mut self,
        // We should name that lifetime due to https://github.com/rust-lang/rust/issues/63033`
        end: QName<'n>,
        buf: &mut Vec<u8>,
    ) -> Result<Span> {
        Ok(read_to_end!(
            self,
            end,
            buf,
            read_event_into_async,
            {
                buf.clear();
            },
            await
        ))
    }

    /// Private function to read until `>` is found. This function expects that
    /// it was called just after encounter a `<` symbol.
    async fn read_until_close_async<'b>(&mut self, buf: &'b mut Vec<u8>) -> Result<Event<'b>> {
        read_until_close!(self, buf, TokioAdapter(&mut self.reader), await)
    }
}

////////////////////////////////////////////////////////////////////////////////////////////////////

impl<R: AsyncBufRead + Unpin> NsReader<R> {
    /// An asynchronous version of [`read_event_into()`]. Reads the next event into
    /// given buffer.
    ///
    /// This method manages namespaces but doesn't resolve them automatically.
    /// You should call [`resolver().resolve_element()`] if you want to get a namespace.
    ///
    /// You also can use [`read_resolved_event_into_async()`] instead if you want
    /// to resolve namespace as soon as you get an event.
    ///
    /// # Examples
    ///
    /// ```
    /// # tokio_test::block_on(async {
    /// # use pretty_assertions::assert_eq;
    /// use quick_xml::events::Event;
    /// use quick_xml::name::{Namespace, ResolveResult::*};
    /// use quick_xml::reader::NsReader;
    ///
    /// let mut reader = NsReader::from_reader(r#"
    ///     <x:tag1 xmlns:x="www.xxxx" xmlns:y="www.yyyy" att1 = "test">
    ///        <y:tag2><!--Test comment-->Test</y:tag2>
    ///        <y:tag2>Test 2</y:tag2>
    ///     </x:tag1>
    /// "#.as_bytes());
    /// reader.config_mut().trim_text(true);
    ///
    /// let mut count = 0;
    /// let mut buf = Vec::new();
    /// let mut txt = Vec::new();
    /// loop {
    ///     match reader.read_event_into_async(&mut buf).await.unwrap() {
    ///         Event::Start(e) => {
    ///             count += 1;
    ///             let (ns, local) = reader.resolver().resolve_element(e.name());
    ///             match local.as_ref() {
    ///                 b"tag1" => assert_eq!(ns, Bound(Namespace(b"www.xxxx"))),
    ///                 b"tag2" => assert_eq!(ns, Bound(Namespace(b"www.yyyy"))),
    ///                 _ => unreachable!(),
    ///             }
    ///         }
    ///         Event::Text(e) => {
    ///             txt.push(e.decode().unwrap().into_owned())
    ///         }
    ///         Event::Eof => break,
    ///         _ => (),
    ///     }
    ///     buf.clear();
    /// }
    /// assert_eq!(count, 3);
    /// assert_eq!(txt, vec!["Test".to_string(), "Test 2".to_string()]);
    /// # }) // tokio_test::block_on
    /// ```
    ///
    /// [`read_event_into()`]: NsReader::read_event_into
    /// [`resolver().resolve_element()`]: crate::name::NamespaceResolver::resolve_element
    /// [`read_resolved_event_into_async()`]: Self::read_resolved_event_into_async
    pub async fn read_event_into_async<'b>(&mut self, buf: &'b mut Vec<u8>) -> Result<Event<'b>> {
        self.pop();
        let event = self.reader.read_event_into_async(buf).await;
        self.process_event(event)
    }

    /// An asynchronous version of [`read_to_end_into()`].
    /// Reads asynchronously until end element is found using provided buffer as
    /// intermediate storage for events content. This function is supposed to be
    /// called after you already read a [`Start`] event.
    ///
    /// See the documentation of [`read_to_end_into()`] for more information.
    ///
    /// # Examples
    ///
    /// This example shows, how you can skip XML content after you read the
    /// start event.
    ///
    /// ```
    /// # tokio_test::block_on(async {
    /// # use pretty_assertions::assert_eq;
    /// use quick_xml::name::{Namespace, ResolveResult};
    /// use quick_xml::events::{BytesStart, Event};
    /// use quick_xml::reader::NsReader;
    ///
    /// let mut reader = NsReader::from_reader(r#"
    ///     <outer xmlns="namespace 1">
    ///         <inner xmlns="namespace 2">
    ///             <outer></outer>
    ///         </inner>
    ///         <inner>
    ///             <inner></inner>
    ///             <inner/>
    ///             <outer></outer>
    ///             <p:outer xmlns:p="ns"></p:outer>
    ///             <outer/>
    ///         </inner>
    ///     </outer>
    /// "#.as_bytes());
    /// reader.config_mut().trim_text(true);
    /// let mut buf = Vec::new();
    ///
    /// let ns = Namespace(b"namespace 1");
    /// let start = BytesStart::from_content(r#"outer xmlns="namespace 1""#, 5);
    /// let end   = start.to_end().into_owned();
    ///
    /// // First, we read a start event...
    /// assert_eq!(
    ///     reader.read_resolved_event_into_async(&mut buf).await.unwrap(),
    ///     (ResolveResult::Bound(ns), Event::Start(start))
    /// );
    ///
    /// // ...then, we could skip all events to the corresponding end event.
    /// // This call will correctly handle nested <outer> elements.
    /// // Note, however, that this method does not handle namespaces.
    /// reader.read_to_end_into_async(end.name(), &mut buf).await.unwrap();
    ///
    /// // At the end we should get an Eof event, because we ate the whole XML
    /// assert_eq!(
    ///     reader.read_resolved_event_into_async(&mut buf).await.unwrap(),
    ///     (ResolveResult::Unbound, Event::Eof)
    /// );
    /// # }) // tokio_test::block_on
    /// ```
    ///
    /// [`read_to_end_into()`]: Self::read_to_end_into
    /// [`Start`]: Event::Start
    pub async fn read_to_end_into_async<'n>(
        &mut self,
        // We should name that lifetime due to https://github.com/rust-lang/rust/issues/63033`
        end: QName<'n>,
        buf: &mut Vec<u8>,
    ) -> Result<Span> {
        // According to the https://www.w3.org/TR/xml11/#dt-etag, end name should
        // match literally the start name. See `Config::check_end_names` documentation
        self.reader.read_to_end_into_async(end, buf).await
    }

    /// An asynchronous version of [`read_resolved_event_into()`]. Reads the next
    /// event into given buffer asynchronously and resolves its namespace (if applicable).
    ///
    /// Namespace is resolved only for [`Start`], [`Empty`] and [`End`] events.
    /// For all other events the concept of namespace is not defined, so
    /// a [`ResolveResult::Unbound`] is returned.
    ///
    /// If you are not interested in namespaces, you can use [`read_event_into_async()`]
    /// which will not automatically resolve namespaces for you.
    ///
    /// # Examples
    ///
    /// ```
    /// # tokio_test::block_on(async {
    /// # use pretty_assertions::assert_eq;
    /// use quick_xml::events::Event;
    /// use quick_xml::name::{Namespace, QName, ResolveResult::*};
    /// use quick_xml::reader::NsReader;
    ///
    /// let mut reader = NsReader::from_reader(r#"
    ///     <x:tag1 xmlns:x="www.xxxx" xmlns:y="www.yyyy" att1 = "test">
    ///        <y:tag2><!--Test comment-->Test</y:tag2>
    ///        <y:tag2>Test 2</y:tag2>
    ///     </x:tag1>
    /// "#.as_bytes());
    /// reader.config_mut().trim_text(true);
    ///
    /// let mut count = 0;
    /// let mut buf = Vec::new();
    /// let mut txt = Vec::new();
    /// loop {
    ///     match reader.read_resolved_event_into_async(&mut buf).await.unwrap() {
    ///         (Bound(Namespace(b"www.xxxx")), Event::Start(e)) => {
    ///             count += 1;
    ///             assert_eq!(e.local_name(), QName(b"tag1").into());
    ///         }
    ///         (Bound(Namespace(b"www.yyyy")), Event::Start(e)) => {
    ///             count += 1;
    ///             assert_eq!(e.local_name(), QName(b"tag2").into());
    ///         }
    ///         (_, Event::Start(_)) => unreachable!(),
    ///
    ///         (_, Event::Text(e)) => {
    ///             txt.push(e.decode().unwrap().into_owned())
    ///         }
    ///         (_, Event::Eof) => break,
    ///         _ => (),
    ///     }
    ///     buf.clear();
    /// }
    /// assert_eq!(count, 3);
    /// assert_eq!(txt, vec!["Test".to_string(), "Test 2".to_string()]);
    /// # }) // tokio_test::block_on
    /// ```
    ///
    /// [`read_resolved_event_into()`]: NsReader::read_resolved_event_into
    /// [`Start`]: Event::Start
    /// [`Empty`]: Event::Empty
    /// [`End`]: Event::End
    /// [`read_event_into_async()`]: Self::read_event_into_async
    pub async fn read_resolved_event_into_async<'ns, 'b>(
        // Name 'ns lifetime, because otherwise we get an error
        // "implicit elided lifetime not allowed here" on ResolveResult
        &'ns mut self,
        buf: &'b mut Vec<u8>,
    ) -> Result<(ResolveResult<'ns>, Event<'b>)> {
        let event = self.read_event_into_async(buf).await?;
        Ok(self.resolver().resolve_event(event))
    }
}

#[cfg(test)]
mod test {
    use super::TokioAdapter;
    use crate::reader::test::check;

    check!(
        #[tokio::test]
        read_event_into_async,
        read_until_close_async,
        TokioAdapter,
        &mut Vec::new(),
        async,
        await
    );

    #[test]
    fn test_future_is_send() {
        // This test should just compile, no actual runtime checks are performed here.
        use super::*;
        use tokio::io::BufReader;
        fn check_send<T: Send>(_: T) {}

        let input = vec![];
        let mut reading_buf = vec![];
        let mut reader = Reader::from_reader(BufReader::new(input.as_slice()));

        check_send(reader.read_event_into_async(&mut reading_buf));
    }
}
