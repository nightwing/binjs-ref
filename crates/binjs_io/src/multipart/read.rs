use std;
use std::cell::RefCell;
use std::io::{ Cursor, Read, Seek };
use std::rc::Rc;

use vec_map::VecMap;

use bytes;
use bytes::compress::*;
use bytes::varnum::*;
use bytes::serialize::*;
use ::TokenReaderError;
use io::*;
use multipart::{ FormatInTable, HEADER_GRAMMAR_TABLE, HEADER_STRINGS_TABLE, HEADER_TREE };
use util::{ PoisonLock, Pos, ReadConst };

impl Into<std::io::Error> for TokenReaderError {
    fn into(self) -> std::io::Error {
        std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{:?}", self))
    }
}

/// Deserialize a bunch of bytes into itself.
struct BufDeserializer;
impl Deserializer for BufDeserializer {
    type Target = Vec<u8>;
    fn read<R: Read + Seek>(&self, reader: &mut R) -> Result<Self::Target, std::io::Error> {
        let size = reader.size();
        let mut buf = Vec::with_capacity(size);
        unsafe { buf.set_len(size); }
        reader.read_exact(&mut buf)?;
        Ok(buf)
    }
}

/// Deserialize a String|null
impl Deserializer for Option<String> {
    type Target = Self;
    fn read<R: Read>(&self, inp: &mut R) -> Result<Self, std::io::Error> {
        let mut byte_len = 0;
        inp.read_varnum(&mut byte_len)?;
        let mut bytes = Vec::with_capacity(byte_len as usize);
        unsafe { bytes.set_len(byte_len as usize); }
        inp.read_exact(&mut bytes)?;
        if &bytes == &[255, 0] {
            Ok(None)
        } else {
            String::from_utf8(bytes)
                .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))
                .map(Some)
        }
    }
}

/// A table of entries indexed by a varnum.
pub struct Table<Value> {
    map: VecMap<Value>,
}
impl<Value> Table<Value> {
    fn get(&self, key: u32) -> Option<&Value> {
        self.map.get(key as usize)
    }
}

/// Deserialize a `Table`.
struct TableDeserializer<D> where D: Deserializer {
    /// The deserializer used for entries in the table.
    deserializer: D,
}

impl<'a, D> Deserializer for TableDeserializer<D> where D: Deserializer, D::Target: FormatInTable {
    type Target = Table<D::Target>;
    fn read<R: Read + Seek>(&self, inp: &mut R) -> Result<Self::Target, std::io::Error> {
        // Get number of entries.
        let mut number_of_entries = 0;
        inp.read_varnum(&mut number_of_entries)?;

        let mut map = VecMap::with_capacity(number_of_entries as usize);

        if D::Target::HAS_LENGTH_INDEX {
            // Read table of lengths.
            let mut byte_lengths = Vec::with_capacity(number_of_entries as usize);
            for _ in 0..number_of_entries {
                let mut byte_len = 0;
                inp.read_varnum(&mut byte_len)?;
                byte_lengths.push(byte_len);
            }

            // Now read each entry.
            for i in 0..number_of_entries as usize {
                let expected_byte_length = byte_lengths[i] as usize;
                let start = inp.pos();
                let value = self.deserializer.read(inp)?;
                let stop = inp.pos();
                if stop - start != expected_byte_length {
                    return Err(TokenReaderError::BadLength {
                        expected: expected_byte_length,
                        got: stop - start
                    }.into());
                }
                map.insert(i, value);
            }
        } else {
            for i in 0..number_of_entries as usize {
                let value = self.deserializer.read(inp)?;
                map.insert(i, value);
            }
        }

        Ok(Table { map })
    }
}

/// Description of a node in the table.
pub struct NodeDescription {
    kind: String,
    fields: Rc<Box<[String]>>
}

impl<'a> FormatInTable for NodeDescription {
    const HAS_LENGTH_INDEX : bool = true;
}

struct NodeDescriptionDeserializer;

/// Deserialize a `NodeDescription`.
///
/// Used as part of the deserialization of `Table<NodeDescription>`.
impl Deserializer for NodeDescriptionDeserializer {
    type Target = NodeDescription;
    fn read<R: Read + Seek>(&self, inp: &mut R) -> Result<Self::Target, std::io::Error> {
        // Extract kind
        let strings_deserializer : Option<String> = None;
        let name = match strings_deserializer.read(inp)? {
            None => return Err(TokenReaderError::EmptyNodeName.into()),
            Some(x) => x
        };

        // Extract fields
        let mut number_of_entries = 0;
        inp.read_varnum(&mut number_of_entries)?;

        let mut fields = Vec::with_capacity(number_of_entries as usize);
        for _ in 0..number_of_entries {
            if let Some(name) = strings_deserializer.read(inp)? {
                fields.push(name);
            } else {
                return Err(TokenReaderError::EmptyFieldName.into())
            }
        }

        Ok(NodeDescription {
            kind: name,
            fields: Rc::new(Box::from(fields))
        })
    }
}

/// The state of the `TreeTokenReader`.
///
/// Use a `PoisonLock` to access this state.
pub struct ReaderState {
    reader: Cursor<Vec<u8>>,
    pub strings_table: Table<Option<String>>,
    pub grammar_table: Table<NodeDescription>,
}

impl ReaderState {
    pub fn position(&mut self) -> u64 {
        self.reader.position()
    }
}

pub struct TreeTokenReader {
    // Shared with all children.
    owner: Rc<RefCell<PoisonLock<ReaderState>>>,
}


impl TreeTokenReader {
    pub fn new<R: Read + Seek>(mut reader: R) -> Result<Self, TokenReaderError> {
        // Check magic headers.
        const MAGIC_HEADER: &'static [u8; 5] = b"BINJS";
        const FORMAT_VERSION: u32 = 0;

        reader.read_const(MAGIC_HEADER)
            .map_err(TokenReaderError::ReadError)?;

        let mut version = 0;
        reader.read_varnum(&mut version)
            .map_err(TokenReaderError::ReadError)?;

        if version != FORMAT_VERSION {
            return Err(TokenReaderError::BadHeader)
        }

        // At this stage, we could start parallelizing reads between grammar table and strings table, possibly even the tree.
        reader.read_const(HEADER_GRAMMAR_TABLE.as_bytes())
            .map_err(TokenReaderError::ReadError)?;

        // Read grammar table
        let grammar_deserializer = TableDeserializer {
            deserializer: NodeDescriptionDeserializer
        };
        let grammar_table = Compression::decompress(&mut reader, &grammar_deserializer)
            .map_err(TokenReaderError::BadCompression)?;

        // Read strings table
        reader.read_const(HEADER_STRINGS_TABLE.as_bytes())
            .map_err(TokenReaderError::ReadError)?;
        let strings_deserializer = TableDeserializer {
            deserializer: None /* Option<String> */
        };
        let strings_table = Compression::decompress(&mut reader, &strings_deserializer)
            .map_err(TokenReaderError::BadCompression)?;

        // Decompress tree section to memory (we could as well stream it)
        reader.read_const(HEADER_TREE.as_bytes())
            .map_err(TokenReaderError::ReadError)?;
        let decompressed_tree = Compression::decompress(&mut reader, &BufDeserializer)
            .map_err(TokenReaderError::BadCompression)?;
        let implem = ReaderState {
            strings_table,
            grammar_table,
            reader: Cursor::new(decompressed_tree)
        };

        Ok(TreeTokenReader {
            owner: Rc::new(RefCell::new(PoisonLock::new(implem)))
        })
    }
}

pub struct SimpleGuard {
    parent: TrivialGuard<TokenReaderError>,
    owner: Rc<RefCell<PoisonLock<ReaderState>>>,
}
impl SimpleGuard {
    fn new(owner: Rc<RefCell<PoisonLock<ReaderState>>>) -> Self {
        SimpleGuard {
            parent: TrivialGuard::new(),
            owner
        }
    }
}
impl Guard for SimpleGuard {
    type Error = TokenReaderError;
    fn done(mut self) -> Result<(), Self::Error> {
        self.parent.finalized = true;
        Ok(())
    }
}
impl Drop for SimpleGuard {
    fn drop(&mut self) {
        debug!(target: "multipart", "Dropping SimpleGuard");
        if self.owner.borrow().is_poisoned() {
            // Don't trigger an assertion failure if we had to bailout because of an exception.
            self.parent.finalized = true;
        }
        // Now `self.parent.drop()` will be called.
    }
}

pub struct ListGuard {
    expected_end: u64,
    start: u64,
    parent: SimpleGuard
}

impl ListGuard {
    fn new(owner: Rc<RefCell<PoisonLock<ReaderState>>>, start: u64, byte_len: u64) -> Self {
        ListGuard {
            parent: SimpleGuard::new(owner),
            start,
            expected_end: start + byte_len,
        }
    }
}
impl Guard for ListGuard {
    type Error = TokenReaderError;
    fn done(mut self) -> Result<(), Self::Error> {
        self.parent.parent.finalized = true;

        let mut owner = self.parent.owner.borrow_mut();
        if owner.is_poisoned() {
            return Ok(())
        }

        let found = owner.try(|state| Ok(state.position()))?;
        if found != self.expected_end {
            owner.poison();
            return Err(TokenReaderError::EndOffsetError {
                start: self.start,
                expected: self.expected_end,
                found,
                description: "list".to_string()
            })
        }

        Ok(())
    }
}
impl Drop for ListGuard {
    fn drop(&mut self) {
        debug!(target: "multipart", "Dropping ListGuard");
        // Now `self.parent.drop()` will be called.
    }
}

impl TokenReader for TreeTokenReader {
    type Error = TokenReaderError;
    type TaggedGuard = SimpleGuard;
    type UntaggedGuard = SimpleGuard;
    type ListGuard = ListGuard;

    fn poison(&mut self) {
        self.owner.borrow_mut().poison();
    }

    fn string(&mut self) -> Result<Option<String>, Self::Error> {
        self.owner.borrow_mut().try(|state| {
            let index = state.reader.read_varnum_2()
                .map_err(TokenReaderError::ReadError)?;
            match state.strings_table.get(index) {
                Some(result) => {
                    debug!(target: "multipart", "Reading string {:?} => {:?}", index, result);
                    Ok(result.clone())
                }
                None => Err(TokenReaderError::BadStringIndex(index))
            }
        })
    }


    /// Read a single `f64`. Note that all numbers are `f64`.
    fn float(&mut self) -> Result<Option<f64>, Self::Error> {
        self.owner.borrow_mut().try(|state| {
            let mut buf : [u8; 8] = unsafe { std::mem::uninitialized() };
            state.reader.read(&mut buf)
                .map_err(TokenReaderError::ReadError)?;
            let result = bytes::float::float_of_bytes(&buf);
            debug!(target: "multipart", "Reading float {:?} => {:?}", buf, result);
            Ok(result)
        })
    }

    /// Read a single `bool`.
    fn bool(&mut self) -> Result<Option<bool>, Self::Error> {
        self.owner.borrow_mut().try(|state| {
            let mut buf : [u8; 1] = unsafe { std::mem::uninitialized() };
            state.reader.read(&mut buf)
                .map_err(TokenReaderError::ReadError)?;
            let result = bytes::bool::bool_of_bytes(&buf)
                .map_err(|_| TokenReaderError::InvalidValue);
            debug!(target: "multipart", "Reading bool {:?} => {:?}", buf, result);
            result
        })
    }

    /// Start reading a list.
    ///
    /// Returns an extractor for that list and the number of elements
    /// in the list. Before dropping the sub-extractor, callers MUST
    /// either reach the end of the list or call `skip()`.
    fn list(&mut self) -> Result<(u32, Self::ListGuard), Self::Error> {
        let clone = self.owner.clone();
        self.owner.borrow_mut().try(move |state| {
            let byte_len = state.reader.read_varnum_2()
                .map_err(TokenReaderError::ReadError)?;
            let guard = ListGuard::new(clone, state.position(), byte_len as u64);
            let list_len = state.reader.read_varnum_2()
                .map_err(TokenReaderError::ReadError)?;
            debug!(target: "multipart", "Reading list with {} items", list_len);
            Ok((list_len, guard))
        })
    }

    /// Start reading a tagged tuple. If the stream was encoded
    /// properly, the tag is attached to an **ordered** tuple of
    /// fields that may be extracted **in order**.
    ///
    /// Returns the tag name, the ordered array of fields in which
    /// the contents must be read, and a sub-extractor dedicated
    /// to that tuple. The sub-extractor MUST be consumed entirely.
    fn tagged_tuple(&mut self) -> Result<(String, Rc<Box<[String]>>, Self::TaggedGuard), Self::Error> {
        let clone = self.owner.clone();
        self.owner.borrow_mut().try(|state| {
            let index = state.reader.read_varnum_2()
                .map_err(TokenReaderError::ReadError)?;
            let description = state.grammar_table.get(index)
                .ok_or(TokenReaderError::BadKindIndex(index))?;

            let tag = description.kind.clone();
            let fields = description.fields.clone();
            let guard = SimpleGuard::new(clone);
            debug!(target: "multipart", "Reading tagged tuple with kind \"{}\", fields {:?}",
                tag, fields);
            Ok((tag, fields, guard))
        })
    }

    /// Start reading an untagged tuple. The sub-extractor MUST
    /// be consumed entirely.
    fn untagged_tuple(&mut self) -> Result<Self::UntaggedGuard, Self::Error> {
        let clone = self.owner.clone();
        debug!(target: "multipart", "Reading untagged tuple");
        Ok(SimpleGuard::new(clone))
    }
}
