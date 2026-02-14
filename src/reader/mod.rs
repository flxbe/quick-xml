//! Contains high-level interface for a pull-based XML parser.

#[cfg(feature = "encoding")]
use encoding_rs::Encoding;
use std::io;
use std::ops::Range;

use crate::encoding::Decoder;
use crate::errors::SyntaxError;
use crate::parser::DtdParser;
use crate::reader::state::ReaderState;

/// A struct that holds a parser configuration.
///
/// Current parser configuration can be retrieved by calling [`Reader::config()`]
/// and changed by changing properties of the object returned by a call to
/// [`Reader::config_mut()`].
///
/// [`Reader::config()`]: crate::reader::Reader::config
/// [`Reader::config_mut()`]: crate::reader::Reader::config_mut
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[cfg_attr(feature = "serde-types", derive(serde::Deserialize, serde::Serialize))]
#[non_exhaustive]
pub struct Config {
    /// Whether lone ampersand character (without a paired semicolon) should be
    /// allowed in textual content. Unless enabled, in case of a dangling ampersand,
    /// the [`Error::IllFormed(UnclosedReference)`] is returned from read methods.
    ///
    /// Default: `false`
    ///
    /// # Example
    ///
    /// ```
    /// # use quick_xml::events::{BytesRef, BytesText, Event};
    /// # use quick_xml::reader::Reader;
    /// # use pretty_assertions::assert_eq;
    /// let mut reader = Reader::from_str("text with & &amp; & alone");
    /// reader.config_mut().allow_dangling_amp = true;
    ///
    /// assert_eq!(reader.read_event().unwrap(), Event::Text(BytesText::new("text with ")));
    /// assert_eq!(reader.read_event().unwrap(), Event::Text(BytesText::from_escaped("& ")));
    /// assert_eq!(reader.read_event().unwrap(), Event::GeneralRef(BytesRef::new("amp")));
    /// assert_eq!(reader.read_event().unwrap(), Event::Text(BytesText::new(" ")));
    /// assert_eq!(reader.read_event().unwrap(), Event::Text(BytesText::from_escaped("& alone")));
    /// assert_eq!(reader.read_event().unwrap(), Event::Eof);
    /// ```
    ///
    /// [`Error::IllFormed(UnclosedReference)`]: crate::errors::IllFormedError::UnclosedReference
    pub allow_dangling_amp: bool,

    /// Whether unmatched closing tag names should be allowed. Unless enabled,
    /// in case of a dangling end tag, the [`Error::IllFormed(UnmatchedEndTag)`]
    /// is returned from read methods.
    ///
    /// When set to `true`, it won't check if a closing tag has a corresponding
    /// opening tag at all. For example, `<a></a></b>` will be permitted.
    ///
    /// Note that the emitted [`End`] event will not be modified if this is enabled,
    /// ie. it will contain the data of the unmatched end tag.
    ///
    /// Note, that setting this to `true` will lead to additional allocates that
    /// needed to store tag name for an [`End`] event.
    ///
    /// Default: `false`
    ///
    /// [`Error::IllFormed(UnmatchedEndTag)`]: crate::errors::IllFormedError::UnmatchedEndTag
    /// [`End`]: crate::events::Event::End
    pub allow_unmatched_ends: bool,

    /// Whether comments should be validated. If enabled, in case of invalid comment
    /// [`Error::IllFormed(DoubleHyphenInComment)`] is returned from read methods.
    ///
    /// When set to `true`, every [`Comment`] event will be checked for not
    /// containing `--`, which [is not allowed] in XML comments. Most of the time
    /// we don't want comments at all so we don't really care about comment
    /// correctness, thus the default value is `false` to improve performance.
    ///
    /// Default: `false`
    ///
    /// [`Error::IllFormed(DoubleHyphenInComment)`]: crate::errors::IllFormedError::DoubleHyphenInComment
    /// [`Comment`]: crate::events::Event::Comment
    /// [is not allowed]: https://www.w3.org/TR/xml11/#sec-comments
    pub check_comments: bool,

    /// Whether mismatched closing tag names should be detected. If enabled, in
    /// case of mismatch the [`Error::IllFormed(MismatchedEndTag)`] is returned from
    /// read methods.
    ///
    /// Note, that start and end tags [should match literally][spec], they cannot
    /// have different prefixes even if both prefixes resolve to the same namespace.
    /// The XML
    ///
    /// ```xml
    /// <outer xmlns="namespace" xmlns:p="namespace">
    /// </p:outer>
    /// ```
    ///
    /// is not valid, even though semantically the start tag is the same as the
    /// end tag. The reason is that namespaces are an extension of the original
    /// XML specification (without namespaces) and it should be backward-compatible.
    ///
    /// When set to `false`, it won't check if a closing tag matches the corresponding
    /// opening tag. For example, `<mytag></different_tag>` will be permitted.
    ///
    /// If the XML is known to be sane (already processed, etc.) this saves extra time.
    ///
    /// Note that the emitted [`End`] event will not be modified if this is disabled,
    /// ie. it will contain the data of the mismatched end tag.
    ///
    /// Note, that setting this to `true` will lead to additional allocates that
    /// needed to store tag name for an [`End`] event. However if [`expand_empty_elements`]
    /// is also set, only one additional allocation will be performed that support
    /// both these options.
    ///
    /// Default: `true`
    ///
    /// [`Error::IllFormed(MismatchedEndTag)`]: crate::errors::IllFormedError::MismatchedEndTag
    /// [spec]: https://www.w3.org/TR/xml11/#dt-etag
    /// [`End`]: crate::events::Event::End
    /// [`expand_empty_elements`]: Self::expand_empty_elements
    pub check_end_names: bool,

    /// Whether empty elements should be split into an `Open` and a `Close` event.
    ///
    /// When set to `true`, all [`Empty`] events produced by a self-closing tag
    /// like `<tag/>` are expanded into a [`Start`] event followed by an [`End`]
    /// event. When set to `false` (the default), those tags are represented by
    /// an [`Empty`] event instead.
    ///
    /// Note, that setting this to `true` will lead to additional allocates that
    /// needed to store tag name for an [`End`] event. However if [`check_end_names`]
    /// is also set, only one additional allocation will be performed that support
    /// both these options.
    ///
    /// Default: `false`
    ///
    /// [`Empty`]: crate::events::Event::Empty
    /// [`Start`]: crate::events::Event::Start
    /// [`End`]: crate::events::Event::End
    /// [`check_end_names`]: Self::check_end_names
    pub expand_empty_elements: bool,

    /// Whether trailing whitespace after the markup name are trimmed in closing
    /// tags `</a >`.
    ///
    /// If `true` the emitted [`End`] event is stripped of trailing whitespace
    /// after the markup name.
    ///
    /// Note that if set to `false` and [`check_end_names`] is `true` the comparison
    /// of markup names is going to fail erroneously if a closing tag contains
    /// trailing whitespace.
    ///
    /// Default: `true`
    ///
    /// [`End`]: crate::events::Event::End
    /// [`check_end_names`]: Self::check_end_names
    pub trim_markup_names_in_closing_tags: bool,

    /// Whether whitespace before character data should be removed.
    ///
    /// When set to `true`, leading whitespace is trimmed in [`Text`] events.
    /// If after that the event is empty it will not be pushed.
    ///
    /// Default: `false`
    ///
    /// <div style="background:rgba(80, 240, 100, 0.20);padding:0.75em;">
    ///
    /// WARNING: With this option every text events will be trimmed which is
    /// incorrect behavior when text events delimited by comments, processing
    /// instructions or CDATA sections. To correctly trim data manually apply
    /// [`BytesText::inplace_trim_start`] and [`BytesText::inplace_trim_end`]
    /// only to necessary events.
    /// </div>
    ///
    /// [`Text`]: crate::events::Event::Text
    /// [`BytesText::inplace_trim_start`]: crate::events::BytesText::inplace_trim_start
    /// [`BytesText::inplace_trim_end`]: crate::events::BytesText::inplace_trim_end
    pub trim_text_start: bool,

    /// Whether whitespace after character data should be removed.
    ///
    /// When set to `true`, trailing whitespace is trimmed in [`Text`] events.
    /// If after that the event is empty it will not be pushed.
    ///
    /// Default: `false`
    ///
    /// <div style="background:rgba(80, 240, 100, 0.20);padding:0.75em;">
    ///
    /// WARNING: With this option every text events will be trimmed which is
    /// incorrect behavior when text events delimited by comments, processing
    /// instructions or CDATA sections. To correctly trim data manually apply
    /// [`BytesText::inplace_trim_start`] and [`BytesText::inplace_trim_end`]
    /// only to necessary events.
    /// </div>
    ///
    /// [`Text`]: crate::events::Event::Text
    /// [`BytesText::inplace_trim_start`]: crate::events::BytesText::inplace_trim_start
    /// [`BytesText::inplace_trim_end`]: crate::events::BytesText::inplace_trim_end
    pub trim_text_end: bool,
}

impl Config {
    /// Set both [`trim_text_start`] and [`trim_text_end`] to the same value.
    ///
    /// <div style="background:rgba(80, 240, 100, 0.20);padding:0.75em;">
    ///
    /// WARNING: With this option every text events will be trimmed which is
    /// incorrect behavior when text events delimited by comments, processing
    /// instructions or CDATA sections. To correctly trim data manually apply
    /// [`BytesText::inplace_trim_start`] and [`BytesText::inplace_trim_end`]
    /// only to necessary events.
    /// </div>
    ///
    /// [`trim_text_start`]: Self::trim_text_start
    /// [`trim_text_end`]: Self::trim_text_end
    /// [`BytesText::inplace_trim_start`]: crate::events::BytesText::inplace_trim_start
    /// [`BytesText::inplace_trim_end`]: crate::events::BytesText::inplace_trim_end
    #[inline]
    pub fn trim_text(&mut self, trim: bool) {
        self.trim_text_start = trim;
        self.trim_text_end = trim;
    }

    /// Turn on or off all checks for well-formedness. Currently it is that settings:
    /// - [`check_comments`](Self::check_comments)
    /// - [`check_end_names`](Self::check_end_names)
    #[inline]
    pub fn enable_all_checks(&mut self, enable: bool) {
        self.check_comments = enable;
        self.check_end_names = enable;
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            allow_dangling_amp: false,
            allow_unmatched_ends: false,
            check_comments: false,
            check_end_names: true,
            expand_empty_elements: false,
            trim_markup_names_in_closing_tags: true,
            trim_text_start: false,
            trim_text_end: false,
        }
    }
}

////////////////////////////////////////////////////////////////////////////////////////////////////

#[cfg(feature = "async-tokio")]
mod async_tokio;
mod buffered_reader;
mod ns_reader;
mod slice_reader;
mod state;

pub use ns_reader::NsReader;

/// Range of input in bytes, that corresponds to some piece of XML
pub type Span = Range<u64>;

////////////////////////////////////////////////////////////////////////////////////////////////////

/// Possible reader states. The state transition diagram (`true` and `false` shows
/// value of [`Config::expand_empty_elements`] option):
///
/// ```mermaid
/// flowchart LR
///   subgraph _
///     direction LR
///
///     Init         -- "(no event)"\n                                       --> InsideMarkup
///     InsideMarkup -- Decl, DocType, PI\nComment, CData\nStart, Empty, End --> InsideText
///     InsideText   -- "#lt;false#gt;\n(no event)"\nText                    --> InsideMarkup
///     InsideRef    -- "(no event)"\nGeneralRef                             --> InsideText
///   end
///   InsideText     -- "#lt;true#gt;"\nStart --> InsideEmpty
///   InsideEmpty    -- End                   --> InsideText
///   _ -. Eof .-> Done
/// ```
#[derive(Clone, Debug)]
enum ParseState {
    /// Initial state in which reader stay after creation. Transition from that
    /// state could produce a `Text`, `Decl`, `Comment` or `Start` event. The next
    /// state is always `InsideMarkup`. The reader will never return to this state. The
    /// event emitted during transition to `InsideMarkup` is a `StartEvent` if the
    /// first symbol not `<`, otherwise no event are emitted.
    Init,
    /// State after seeing the `&` symbol in textual content. Depending on the next symbol all other
    /// events could be generated.
    ///
    /// After generating one event the reader moves to the `ClosedTag` state.
    InsideRef,
    /// State after seeing the `<` symbol. Depending on the next symbol all other
    /// events could be generated.
    ///
    /// After generating one event the reader moves to the `InsideText` state.
    InsideMarkup,
    /// State in which reader searches the `<` symbol of a markup. All bytes before
    /// that symbol will be returned in the [`Event::Text`] event. After that
    /// the reader moves to the `InsideMarkup` state.
    InsideText,
    /// This state is used only if option [`expand_empty_elements`] is set to `true`.
    /// Reader enters to this state when it is in a `InsideText` state and emits an
    /// [`Event::Start`] event. The next event emitted will be an [`Event::End`],
    /// after which reader returned to the `InsideText` state.
    ///
    /// [`expand_empty_elements`]: Config::expand_empty_elements
    InsideEmpty,
    /// Reader enters this state when `Eof` event generated or an error occurred.
    /// This is the last state, the reader stay in it forever.
    Done,
}

/// A reference to an encoding together with information about how it was retrieved.
///
/// The state transition diagram:
///
/// ```mermaid
/// flowchart LR
///   Implicit    -- from_str       --> Explicit
///   Implicit    -- BOM            --> BomDetected
///   Implicit    -- "encoding=..." --> XmlDetected
///   BomDetected -- "encoding=..." --> XmlDetected
/// ```
#[cfg(feature = "encoding")]
#[derive(Clone, Copy, Debug)]
enum EncodingRef {
    /// Encoding was implicitly assumed to have a specified value. It can be refined
    /// using BOM or by the XML declaration event (`<?xml encoding=... ?>`)
    Implicit(&'static Encoding),
    /// Encoding was explicitly set to the desired value. It cannot be changed
    /// nor by BOM, nor by parsing XML declaration (`<?xml encoding=... ?>`)
    Explicit(&'static Encoding),
    /// Encoding was detected from a byte order mark (BOM) or by the first bytes
    /// of the content. It can be refined by the XML declaration event (`<?xml encoding=... ?>`)
    BomDetected(&'static Encoding),
    /// Encoding was detected using XML declaration event (`<?xml encoding=... ?>`).
    /// It can no longer change
    XmlDetected(&'static Encoding),
}
#[cfg(feature = "encoding")]
impl EncodingRef {
    #[inline]
    const fn encoding(&self) -> &'static Encoding {
        match self {
            Self::Implicit(e) => e,
            Self::Explicit(e) => e,
            Self::BomDetected(e) => e,
            Self::XmlDetected(e) => e,
        }
    }
    #[inline]
    const fn can_be_refined(&self) -> bool {
        match self {
            Self::Implicit(_) | Self::BomDetected(_) => true,
            Self::Explicit(_) | Self::XmlDetected(_) => false,
        }
    }
}

////////////////////////////////////////////////////////////////////////////////////////////////////

/// A direct stream to the underlying [`Reader`]s reader which updates
/// [`Reader::buffer_position()`] when read from it.
#[derive(Debug)]
#[must_use = "streams do nothing unless read or polled"]
pub struct BinaryStream<'r, R> {
    inner: &'r mut R,
    offset: &'r mut u64,
}

impl<'r, R> BinaryStream<'r, R> {
    /// Returns current position in bytes in the original source.
    #[inline]
    pub const fn offset(&self) -> u64 {
        *self.offset
    }

    /// Gets a reference to the underlying reader.
    #[inline]
    pub const fn get_ref(&self) -> &R {
        self.inner
    }

    /// Gets a mutable reference to the underlying reader.
    ///
    /// Avoid read from this reader because this will not update reader's position
    /// and will lead to incorrect positions of errors. Read from this stream instead.
    #[inline]
    pub fn get_mut(&mut self) -> &mut R {
        self.inner
    }
}

impl<'r, R> io::Read for BinaryStream<'r, R>
where
    R: io::Read,
{
    #[inline]
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let amt: usize = self.inner.read(buf)?;
        *self.offset += amt as u64;
        Ok(amt)
    }
}

impl<'r, R> io::BufRead for BinaryStream<'r, R>
where
    R: io::BufRead,
{
    #[inline]
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        self.inner.fill_buf()
    }

    #[inline]
    fn consume(&mut self, amt: usize) {
        self.inner.consume(amt);
        *self.offset += amt as u64;
    }
}

////////////////////////////////////////////////////////////////////////////////////////////////////

/// A low level encoding-agnostic XML event reader.
///
/// Consumes bytes and streams XML [`Event`]s.
///
/// This reader does not manage namespace declarations and not able to resolve
/// prefixes. If you want these features, use the [`NsReader`].
///
/// # Examples
///
/// ```
/// use quick_xml::events::Event;
/// use quick_xml::reader::Reader;
///
/// let xml = r#"<tag1 att1 = "test">
///                 <tag2><!--Test comment-->Test</tag2>
///                 <tag2>Test 2</tag2>
///              </tag1>"#;
/// let mut reader = Reader::from_str(xml);
/// reader.config_mut().trim_text(true);
///
/// let mut count = 0;
/// let mut txt = Vec::new();
/// let mut buf = Vec::new();
///
/// // The `Reader` does not implement `Iterator` because it outputs borrowed data (`Cow`s)
/// loop {
///     // NOTE: this is the generic case when we don't know about the input BufRead.
///     // when the input is a &str or a &[u8], we don't actually need to use another
///     // buffer, we could directly call `reader.read_event()`
///     match reader.read_event_into(&mut buf) {
///         Err(e) => panic!("Error at position {}: {:?}", reader.error_position(), e),
///         // exits the loop when reaching end of file
///         Ok(Event::Eof) => break,
///
///         Ok(Event::Start(e)) => {
///             match e.name().as_ref() {
///                 b"tag1" => println!("attributes values: {:?}",
///                                     e.attributes().map(|a| a.unwrap().value)
///                                     .collect::<Vec<_>>()),
///                 b"tag2" => count += 1,
///                 _ => (),
///             }
///         }
///         Ok(Event::Text(e)) => txt.push(e.decode().unwrap().into_owned()),
///
///         // There are several other `Event`s we do not consider here
///         _ => (),
///     }
///     // if we don't keep a borrow elsewhere, we can clear the buffer to keep memory usage low
///     buf.clear();
/// }
/// ```
///
/// [`NsReader`]: crate::reader::NsReader
#[derive(Debug, Clone)]
pub struct Reader<R> {
    /// Source of data for parse
    reader: R,
    /// Configuration and current parse state
    state: ReaderState,
}

/// Builder methods
impl<R> Reader<R> {
    /// Creates a `Reader` that reads from a given reader.
    pub fn from_reader(reader: R) -> Self {
        Self {
            reader,
            state: ReaderState::default(),
        }
    }

    /// Returns reference to the parser configuration
    pub const fn config(&self) -> &Config {
        &self.state.config
    }

    /// Returns mutable reference to the parser configuration
    pub fn config_mut(&mut self) -> &mut Config {
        &mut self.state.config
    }
}

/// Getters
impl<R> Reader<R> {
    /// Consumes `Reader` returning the underlying reader
    ///
    /// Can be used to compute line and column of a parsing error position
    ///
    /// # Examples
    ///
    /// ```
    /// # use pretty_assertions::assert_eq;
    /// use std::{str, io::Cursor};
    /// use quick_xml::events::Event;
    /// use quick_xml::reader::Reader;
    ///
    /// let xml = r#"<tag1 att1 = "test">
    ///                 <tag2><!--Test comment-->Test</tag2>
    ///                 <tag3>Test 2</tag3>
    ///              </tag1>"#;
    /// let mut reader = Reader::from_reader(Cursor::new(xml.as_bytes()));
    /// let mut buf = Vec::new();
    ///
    /// fn into_line_and_column(reader: Reader<Cursor<&[u8]>>) -> (usize, usize) {
    ///     // We known that size cannot exceed usize::MAX because we created parser from single &[u8]
    ///     let end_pos = reader.buffer_position() as usize;
    ///     let mut cursor = reader.into_inner();
    ///     let s = String::from_utf8(cursor.into_inner()[0..end_pos].to_owned())
    ///         .expect("can't make a string");
    ///     let mut line = 1;
    ///     let mut column = 0;
    ///     for c in s.chars() {
    ///         if c == '\n' {
    ///             line += 1;
    ///             column = 0;
    ///         } else {
    ///             column += 1;
    ///         }
    ///     }
    ///     (line, column)
    /// }
    ///
    /// loop {
    ///     match reader.read_event_into(&mut buf) {
    ///         Ok(Event::Start(ref e)) => match e.name().as_ref() {
    ///             b"tag1" | b"tag2" => (),
    ///             tag => {
    ///                 assert_eq!(b"tag3", tag);
    ///                 assert_eq!((3, 22), into_line_and_column(reader));
    ///                 break;
    ///             }
    ///         },
    ///         Ok(Event::Eof) => unreachable!(),
    ///         _ => (),
    ///     }
    ///     buf.clear();
    /// }
    /// ```
    pub fn into_inner(self) -> R {
        self.reader
    }

    /// Gets a reference to the underlying reader.
    pub const fn get_ref(&self) -> &R {
        &self.reader
    }

    /// Gets a mutable reference to the underlying reader.
    ///
    /// Avoid read from this reader because this will not update reader's position
    /// and will lead to incorrect positions of errors. If you want to read, use
    /// [`stream()`] instead.
    ///
    /// [`stream()`]: Self::stream
    pub fn get_mut(&mut self) -> &mut R {
        &mut self.reader
    }

    /// Gets the byte position in the input data just after the last emitted event
    /// (i.e. this is position where data of last event ends).
    ///
    /// Note, that for text events which is originally ended with whitespace characters
    /// (` `, `\t`, `\r`, and `\n`) if [`Config::trim_text_end`] is set this is position
    /// before trim, not the position of the last byte of the [`Event::Text`] content.
    pub const fn buffer_position(&self) -> u64 {
        // when internal state is InsideMarkup, we have actually read until '<',
        // which we don't want to show
        if let ParseState::InsideMarkup = self.state.state {
            self.state.offset - 1
        } else {
            self.state.offset
        }
    }

    /// Gets the last error byte position in the input data. If there is no errors
    /// yet, returns `0`.
    ///
    /// Unlike `buffer_position` it will point to the place where it is rational
    /// to report error to the end user. For example, all [`SyntaxError`]s are
    /// reported when the parser sees EOF inside of some kind of markup. The
    /// `buffer_position()` will point to the last byte of input which is not
    /// very useful. `error_position()` will point to the start of corresponding
    /// markup element (i. e. to the `<` character).
    ///
    /// This position is always `<= buffer_position()`.
    pub const fn error_position(&self) -> u64 {
        self.state.last_error_offset
    }

    /// Get the decoder, used to decode bytes, read by this reader, to the strings.
    ///
    /// If [`encoding`] feature is enabled, the used encoding may change after
    /// parsing the XML declaration, otherwise encoding is fixed to UTF-8.
    ///
    /// If [`encoding`] feature is enabled and no encoding is specified in declaration,
    /// defaults to UTF-8.
    ///
    /// [`encoding`]: ../index.html#encoding
    #[inline]
    pub const fn decoder(&self) -> Decoder {
        self.state.decoder()
    }

    /// Get the direct access to the underlying reader, but tracks the amount of
    /// read data and update [`Reader::buffer_position()`] accordingly.
    ///
    /// Note, that this method gives you access to the internal reader and read
    /// data will not be returned in any subsequent events read by `read_event`
    /// family of methods.
    ///
    /// # Example
    ///
    /// This example demonstrates how to read stream raw bytes from an XML document.
    /// This could be used to implement streaming read of text, or to read raw binary
    /// bytes embedded in an XML document. (Documents with embedded raw bytes are not
    /// valid XML, but XML-derived file formats exist where such documents are valid).
    ///
    /// ```
    /// # use pretty_assertions::assert_eq;
    /// use std::io::{BufRead, Read};
    /// use quick_xml::events::{BytesEnd, BytesStart, Event};
    /// use quick_xml::reader::Reader;
    ///
    /// let mut reader = Reader::from_str("<tag>binary << data&></tag>");
    /// //                                 ^    ^               ^     ^
    /// //                                 0    5              21    27
    ///
    /// assert_eq!(
    ///     (reader.read_event().unwrap(), reader.buffer_position()),
    ///     // 5 - end of the `<tag>`
    ///     (Event::Start(BytesStart::new("tag")), 5)
    /// );
    ///
    /// // Reading directly from underlying reader will not update position
    /// // let mut inner = reader.get_mut();
    ///
    /// // Reading from the stream() advances position
    /// let mut inner = reader.stream();
    ///
    /// // Read binary data. We must know its size
    /// let mut binary = [0u8; 16];
    /// inner.read_exact(&mut binary).unwrap();
    /// assert_eq!(&binary, b"binary << data&>");
    /// // 21 - end of the `binary << data&>`
    /// assert_eq!(inner.offset(), 21);
    /// assert_eq!(reader.buffer_position(), 21);
    ///
    /// assert_eq!(
    ///     (reader.read_event().unwrap(), reader.buffer_position()),
    ///     // 27 - end of the `</tag>`
    ///     (Event::End(BytesEnd::new("tag")), 27)
    /// );
    ///
    /// assert_eq!(reader.read_event().unwrap(), Event::Eof);
    /// ```
    #[inline]
    pub fn stream(&mut self) -> BinaryStream<'_, R> {
        BinaryStream {
            inner: &mut self.reader,
            offset: &mut self.state.offset,
        }
    }
}

////////////////////////////////////////////////////////////////////////////////////////////////////

/// Possible elements started with `<!`
#[derive(Debug, PartialEq)]
enum BangType {
    /// <![CDATA[...]]>
    CData,
    /// <!--...-->
    Comment,
    /// <!DOCTYPE...>. Contains balance of '<' (+1) and '>' (-1)
    DocType(DtdParser),
}
impl BangType {
    #[inline(always)]
    const fn new(byte: Option<u8>) -> Result<Self, SyntaxError> {
        Ok(match byte {
            Some(b'[') => Self::CData,
            Some(b'-') => Self::Comment,
            Some(b'D') | Some(b'd') => Self::DocType(DtdParser::BeforeInternalSubset(0)),
            _ => return Err(SyntaxError::InvalidBangMarkup),
        })
    }

    /// If element is finished, returns its content up to `>` symbol and
    /// an index of this symbol, otherwise returns `None`
    ///
    /// # Parameters
    /// - `buf`: buffer with data consumed on previous iterations
    /// - `chunk`: data read on current iteration and not yet consumed from reader
    #[inline(always)]
    fn parse<'b>(&mut self, buf: &[u8], chunk: &'b [u8]) -> Option<(&'b [u8], usize)> {
        match self {
            Self::Comment => {
                for i in memchr::memchr_iter(b'>', chunk) {
                    // Need to read at least 6 symbols (`!---->`) for properly finished comment
                    // <!----> - XML comment
                    //  012345 - i
                    if buf.len() + i > 4 {
                        if chunk[..i].ends_with(b"--") {
                            // We cannot strip last `--` from the buffer because we need it in case of
                            // check_comments enabled option. XML standard requires that comment
                            // will not end with `--->` sequence because this is a special case of
                            // `--` in the comment (https://www.w3.org/TR/xml11/#sec-comments)
                            return Some((&chunk[..i], i + 1)); // +1 for `>`
                        }
                        // End sequence `-|->` was splitted at |
                        //        buf --/   \-- chunk
                        if i == 1 && buf.ends_with(b"-") && chunk[0] == b'-' {
                            return Some((&chunk[..i], i + 1)); // +1 for `>`
                        }
                        // End sequence `--|>` was splitted at |
                        //         buf --/   \-- chunk
                        if i == 0 && buf.ends_with(b"--") {
                            return Some((&[], i + 1)); // +1 for `>`
                        }
                    }
                }
            }
            Self::CData => {
                for i in memchr::memchr_iter(b'>', chunk) {
                    if chunk[..i].ends_with(b"]]") {
                        return Some((&chunk[..i], i + 1)); // +1 for `>`
                    }
                    // End sequence `]|]>` was splitted at |
                    //        buf --/   \-- chunk
                    if i == 1 && buf.ends_with(b"]") && chunk[0] == b']' {
                        return Some((&chunk[..i], i + 1)); // +1 for `>`
                    }
                    // End sequence `]]|>` was splitted at |
                    //         buf --/   \-- chunk
                    if i == 0 && buf.ends_with(b"]]") {
                        return Some((&[], i + 1)); // +1 for `>`
                    }
                }
            }
            Self::DocType(ref mut parser) => return parser.feed(buf, chunk),
        }
        None
    }
    #[inline]
    const fn to_err(&self) -> SyntaxError {
        match self {
            Self::CData => SyntaxError::UnclosedCData,
            Self::Comment => SyntaxError::UnclosedComment,
            Self::DocType(_) => SyntaxError::UnclosedDoctype,
        }
    }
}
