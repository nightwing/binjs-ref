use bytes;
use bytes::compress::*;
use bytes::varnum::*;
use io::*;
use ::TokenWriterError;
use multipart::*;

use std;
use std::collections::{ HashMap, HashSet };
use std::cell::RefCell;
use std::fmt::{ Display, Formatter };
use std::hash::Hash;
use std::io::Write;
use std::ops::{ Add, AddAssign };
use std::rc::Rc;

use rand::{ Rand, Rng };

use vec_map;
use vec_map::*;

#[derive(Clone, Debug)]
pub struct WriteOptions {
    pub grammar_table: Compression,
    pub strings_table: Compression,
    pub tree: Compression,
}

impl Rand for WriteOptions {
    fn rand<R: Rng>(rng: &mut R) -> Self {
        WriteOptions {
            grammar_table: Compression::rand(rng),
            strings_table: Compression::rand(rng),
            tree: Compression::rand(rng),
        }
    }
}


/// A value that may be serialized to bytes, optionally compressed.
trait Serializable {
    /// Write the data, without compression.
    fn write<W: Write>(&self, &mut W) -> Result<usize, std::io::Error>;

    /// Write the data, with compression.
    fn write_with_compression<W: Write>(&self, out: &mut W, compression: &Compression) -> Result<CompressionResult, std::io::Error> {
        let mut uncompressed = Vec::with_capacity(2048);
        self.write(&mut uncompressed)?;
        compression.compress(&uncompressed, out)
    }
}

impl Serializable for Vec<u8> {
    fn write<W: Write>(&self, out: &mut W) -> Result<usize, std::io::Error> {
        out.write_all(&self)?;
        Ok(self.len())
    }
}

/// A `String` is serialized as:
/// - number of UTF-8 bytes (varnum);
/// - sequence of UTF-8 bytes.
impl Serializable for String {
    fn write<W: Write>(&self, out: &mut W) -> Result<usize, std::io::Error> {
        let mut total = 0;
        total += out.write_varnum(self.len() as u32)?;
        out.write_all(self.as_bytes())?;
        total += self.len();
        Ok(total)
    }
}

/// A `String | null` is serialized as:
/// - number of UTF-8 bytes (varnum);
/// - sequence of UTF-8 bytes.
///
/// With the following special case used to represent the null string:
/// - number of UTF-8 bytes (2 as varnum);
/// - sequence [255, 0] (which is invalid UTF-8).
impl Serializable for Option<String> {
    fn write<W: Write>(&self, out: &mut W) -> Result<usize, std::io::Error> {
        const EMPTY_STRING: [u8; 2] = [255, 0];
        let total = match *self {
            None => {
                let mut total = 0;
                total += out.write_varnum(EMPTY_STRING.len() as u32)?;
                out.write_all(&EMPTY_STRING)?;
                total += EMPTY_STRING.len();
                total
            },
            Some(ref data) => {
                data.write(out)?
            }
        };
        Ok(total)
    }
}


/// An entry in an WriterTable.
///
/// This entry tracks the number of instances of the entry used in the table.
struct TableEntry<T> where T: Clone { // We shouldn't need the `Clone`, sigh.
    /// Number of instances of this entry around.
    instances: RefCell<u32>,

    /// The actual data.
    data: T,

    /// The index, actually computed in `write()`.
    index: TableIndex<T>
}
impl<T> TableEntry<T> where T: Clone {
    fn new(data: T) -> Self {
        TableEntry {
            instances: RefCell::new(1),
            data,
            index: TableIndex::new()
        }
    }
}

/// A table, used to define a varnum-indexed header
struct WriterTable<Entry> where Entry: Eq + Hash + Clone + Serializable + FormatInTable {
    map: HashMap<Entry, TableEntry<Entry>>
}

impl<Entry> WriterTable<Entry> where Entry: Eq + Hash + Clone + Serializable + FormatInTable {
    pub fn new() -> Self {
        WriterTable {
            map: HashMap::new()
        }
    }
}


impl<Entry> WriterTable<Entry> where Entry: Eq + Hash + Clone + Serializable + FormatInTable {
    /// Get an entry from the header.
    ///
    /// The number of entries is incremented by 1.
    fn get(&self, kind: &Entry) -> Option<&TableEntry<Entry>> {
        self.map.get(kind)
            .map(|entry| {
                // Increment by 1
                let mut borrow = entry.instances.borrow_mut();
                *borrow += 1;
                entry
            })
    }

    /// Insert an entry.
    ///
    /// If the entry is already present, increment its number instances of 1.
    fn insert(&mut self, entry: Entry) -> TableIndex<Entry> {
        use std::collections::hash_map::Entry::*;
        match self.map.entry(entry) {
            Occupied(slot) => {
                *slot.get().instances.borrow_mut() += 1;
                slot.get().index.clone()
            }
            Vacant(slot) => {
                let entry = TableEntry::new(slot.key().clone());
                let index = entry.index.clone();
                slot.insert(entry);
                index
            }
        }
    }
}

/// An WriterTable is serialized as
///
/// - number of entries (varnum);
/// - if the type of Values does not contain its own length index
///    - for each entry,
///       -   byte length of entry (varnum);
/// - for each entry,
/// -   serialization of entry.
impl<Entry> Serializable for WriterTable<Entry> where Entry: Eq + Hash + Clone + Serializable + FormatInTable {
    fn write<W: Write>(&self, out: &mut W) -> Result<usize, std::io::Error> {
        let mut total = 0;

        // Sort entries by number of uses.
        let mut contents : Vec<_> = self.map.values().collect();
        contents.sort_unstable_by(|a, b| u32::cmp(&*b.instances.borrow(), &*a.instances.borrow()));

        // Assign TableIndex
        for i in 0..contents.len() {
            let mut borrow = contents[i].index.index.borrow_mut();
            *borrow = Some(i as u32);
        }

        // Serialize each entry
        let mut serialized = Vec::with_capacity(contents.len());
        for entry in contents.drain(..) {
            let mut bytes = Vec::with_capacity(256);
            entry.data.write(&mut bytes)?;
            serialized.push(bytes);
        }

        // Write number of entries
        total += out.write_varnum(serialized.len() as u32)?;

        if Entry::HAS_LENGTH_INDEX {
            // Write length of each entry
            for entry in &serialized {
                total += out.write_varnum(entry.len() as u32)?;
            }
        }

        // Write actual content of each entry
        for entry in &serialized {
            out.write_all(&entry)?;
            total += entry.len()
        }

        // Sanity check
        for entry in self.map.values() {
            debug_assert!(entry.index.index.borrow().is_some())
        }

        Ok(total)
    }
}


#[derive(PartialEq, Eq, Clone, Hash, Debug)] // FIXME: Clone shouldn't be necessary. Sigh.
pub struct NodeDescription {
    kind: String,
    fields: Vec<String>, // FIXME: Atoms would be nice here.
}

/// Format:
/// - kind name (see Option<String>);
/// - number of fields (varnum);
/// - for each field
///    - field name (see Option<String>)
impl Serializable for NodeDescription {
    fn write<W: Write>(&self, out: &mut W) -> Result<usize, std::io::Error> {
        let mut total = 0;

        total += self.kind.to_string().write(out)?;
        total += out.write_varnum(self.fields.len() as u32)?;
        for field in &self.fields {
            total += field.to_string().write(out)?;
        }
        Ok(total)
    }
}

impl FormatInTable for NodeDescription {
    const HAS_LENGTH_INDEX : bool = true;
}

enum Nature {
    ListHeader,
    ListWhole,
    TaggedTupleHeader,
    TaggedTupleWhole,
    UntaggedTuple,
    Float,
    Bool,
    String,
}

enum Item {
    String(TableIndex<Option<String>>),
    NodeDescription(TableIndex<NodeDescription>),
    Encoded(Vec<u8>),
    List(Vec<Rc<LabelledItem>>)
}

struct LabelledItem {
    item: Item,
    nature: Nature,
}

#[derive(Clone)]
pub struct Tree(Rc<LabelledItem>);

impl LabelledItem {
    fn write<W: Write>(&self, out: &mut W, stats: &mut Statistics) -> Result<usize, std::io::Error> {
        match self.item {
            Item::String(ref index) => {
                let result = index.write(out)?;
                stats.string.entries += 1;
                stats.string.own_bytes += result;
                stats.string.total_bytes += result;
                Ok(result)
            }
            Item::NodeDescription(ref index) => {
                let result = index.write(out)?;
                for stat in &mut [&mut stats.tagged_tuple, &mut stats.tagged_header] {
                    stat.entries += 1;
                    stat.own_bytes += result;
                    stat.total_bytes += result;
                }
                Ok(result)
            },
            Item::Encoded(ref vec) => {
                out.write_all(&vec)?;
                let result = vec.len();
                let stats = match self.nature {
                    Nature::Bool => Some(&mut stats.bool),
                    Nature::Float => Some(&mut stats.float),
                    Nature::ListHeader => Some(&mut stats.list_header),
                    _ => None
                };
                if let Some(stats) = stats {
                    stats.entries += 1;
                    stats.own_bytes += result;
                    stats.total_bytes += result;
                    stats.shallow_bytes += result;
                }
                Ok(result)
            },
            Item::List(ref items) => {
                let mut shallow_bytes = 0;
                let mut total_bytes = 0;
                match self.nature {
                    Nature::ListWhole => {
                        // Compute byte length
                        let mut buf = Vec::with_capacity(1024);
                        for item in items {
                            let len = item.write(&mut buf, stats)?;
                            if let Item::List(_) = item.item {
                                // These bytes are not part of the shallow count.
                            } else {
                                shallow_bytes += len;
                            }
                        }
                        // Write byte length
                        let bytelen_len = out.write_varnum(buf.len() as u32)?;
                        total_bytes += bytelen_len;
                        // Write data
                        out.write_all(&buf)?;
                        total_bytes += buf.len();

                        // Update statistics
                        stats.list.entries += 1;
                        stats.list.own_bytes += bytelen_len;
                        stats.list.total_bytes += total_bytes;
                        stats.list.shallow_bytes += shallow_bytes;

                        stats.list_header.own_bytes += bytelen_len;
                        stats.list_header.total_bytes += bytelen_len;
                        stats.list_header.shallow_bytes += bytelen_len;

                        match stats.list_lengths.entry(items.len()) {
                            vec_map::Entry::Occupied(mut entry) => {
                                let borrow = entry.get_mut();
                                *borrow += 1;
                            }
                            vec_map::Entry::Vacant(entry) => {
                                entry.insert(1);
                            }
                        }
                    }
                    Nature::TaggedTupleWhole => {
                        assert!(items.len() > 0);

                        // Size of the first element. Useful for statistics.
                        let mut first_size = None;
                        for item in items {
                            let len = item.write(out, stats)?;
                            if first_size.is_none() {
                                first_size = Some(len);
                            }
                            total_bytes += len;
                            if let Item::List(_) = item.item {
                                // These bytes are not part of the shallow count.
                            } else {
                                shallow_bytes += len;
                            }
                        }
                        let first_size = first_size.unwrap(); // We checked above that `items.len() > 0`.
                        stats.tagged_tuple.entries += 1;
                        stats.tagged_tuple.total_bytes += total_bytes;
                        stats.tagged_tuple.shallow_bytes += shallow_bytes;

                        // Update statistics.
                        if let LabelledItem {
                            nature: Nature::TaggedTupleHeader,
                            item: Item::NodeDescription(ref index)
                        } = *items[0] {
                            let key = index.index.borrow()
                                .expect("TableIndex hasn't been resolved");

                            match stats.per_kind_index.entry(key as usize) {
                                vec_map::Entry::Occupied(mut entry) => {
                                    let borrow = entry.get_mut();
                                    borrow.entries += 1;
                                    borrow.total_bytes += total_bytes;
                                    borrow.own_bytes += first_size;
                                    borrow.shallow_bytes += shallow_bytes;
                                }
                                vec_map::Entry::Vacant(entry) => {
                                    entry.insert(NodeStatistics {
                                        entries: 1,
                                        max_entries: 1,
                                        shallow_bytes,
                                        total_bytes,
                                        own_bytes: first_size,
                                    });
                                }
                            }
                        } else {
                            panic!("Internal error: Tagged tuple doesn't have the expected structure")
                        }
                    }
                    _ => {
                        for item in items {
                            total_bytes += item.write(out, stats)?;
                        }
                    }
                }
                Ok(total_bytes)
            }
        }
    }
}

impl Tree {
    fn write<W: Write>(&self, out: &mut W, stats: &mut Statistics) -> Result<usize, std::io::Error> {
        self.0.write(out, stats)
    }
}

#[derive(Debug)]
struct TableIndex<T> {
    phantom: std::marker::PhantomData<T>,
    index: Rc<RefCell<Option<u32>>>,
}

impl<T> Clone for TableIndex<T> {
    fn clone(&self) -> Self {
        TableIndex {
            phantom: std::marker::PhantomData,
            index: self.index.clone()
        }
    }
}
impl<T> TableIndex<T> {
    fn new() -> Self {
        TableIndex {
            phantom: std::marker::PhantomData,
            index: Rc::new(RefCell::new(None))
        }
    }
}
impl<T> Serializable for TableIndex<T> {
    fn write<W: Write>(&self, out: &mut W) -> Result<usize, std::io::Error> {
        if let Some(ref i) = *self.index.borrow() {
            out.write_varnum(*i)
        } else {
            panic!("Attempting to serialize a TableIndex whose index is None");
        }
    }
}



impl TreeTokenWriter {
    pub fn new(options: WriteOptions) -> Self {
        TreeTokenWriter {
            grammar_table: WriterTable::new(),
            strings_table: WriterTable::new(),
            root: None,
            data: Vec::with_capacity(1024),
            options,
            statistics: Statistics::default()
        }
    }

    fn register(&mut self, data: LabelledItem) -> Tree {
        let result = Rc::new(data);
        self.root = Some(Tree(result.clone()));
        Tree(result)
    }

    pub fn done(mut self) -> Result<(Box<[u8]>, Statistics), TokenWriterError> {
        const MAGIC_HEADER: &[u8; 5] = b"BINJS";
        // Write header to byte stream
        self.data.write_all(MAGIC_HEADER)
            .map_err(TokenWriterError::WriteError)?;
        self.statistics.uncompressed_bytes += MAGIC_HEADER.len();

        const FORMAT_VERSION : u32 = 0;
        self.data.write_varnum(FORMAT_VERSION)
            .map_err(TokenWriterError::WriteError)?;
        self.statistics.uncompressed_bytes += std::mem::size_of_val(&FORMAT_VERSION);

        // Write grammar table to byte stream.
        self.data.write_all(HEADER_GRAMMAR_TABLE.as_bytes())
            .map_err(TokenWriterError::WriteError)?;
        self.statistics.uncompressed_bytes += HEADER_GRAMMAR_TABLE.len();
        let compression = self.grammar_table.write_with_compression(&mut self.data, &self.options.grammar_table)
            .map_err(TokenWriterError::WriteError)?;
        self.statistics.grammar_table.entries = self.grammar_table.map.len();
        self.statistics.grammar_table.max_entries = self.grammar_table.map.len();
        self.statistics.grammar_table.compression = compression;

        // Write strings table to byte stream.
        self.data.write_all(HEADER_STRINGS_TABLE.as_bytes())
            .map_err(TokenWriterError::WriteError)?;
        let compression = self.strings_table.write_with_compression(&mut self.data, &self.options.strings_table)
            .map_err(TokenWriterError::WriteError)?;
        self.statistics.strings_table.entries = self.strings_table.map.len();
        self.statistics.strings_table.max_entries = self.strings_table.map.len();
        self.statistics.strings_table.compression = compression;

        // Compute more statistics on strings.
        for (key, value) in &self.strings_table.map {
            let len = match *key {
                None => 0,
                Some(ref s) => s.len()
            };
            match self.statistics.string_lengths.entry(len) {
                vec_map::Entry::Occupied(mut entry) => {
                    let borrow = entry.get_mut();
                    *borrow += 1;
                }
                vec_map::Entry::Vacant(entry) => {
                    entry.insert(1);
                }
            }

            match self.statistics.string_usage.entry(value.instances.borrow().clone() as usize) {
                vec_map::Entry::Occupied(mut entry) => {
                    let borrow = entry.get_mut();
                    *borrow += 1;
                }
                vec_map::Entry::Vacant(entry) => {
                    entry.insert(1);
                }
            }
        }


        // Write tree itself to byte stream.
        self.data.write_all(HEADER_TREE.as_bytes())
            .map_err(TokenWriterError::WriteError)?;
        if let Some(ref root) = self.root {
            let mut buf = Vec::with_capacity(2048);
            root.write(&mut buf, &mut self.statistics)
                .map_err(TokenWriterError::WriteError)?;
            let compression = buf.write_with_compression(&mut self.data, &self.options.tree)
                .map_err(TokenWriterError::WriteError)?;
            self.statistics.tree.entries = 1;
            self.statistics.tree.max_entries = 1;
            self.statistics.tree.compression = compression;
        }

        // Compute more statistics on nodes.
        for (key, value) in self.grammar_table.map {
            let index = value.index.index.borrow()
                .expect("Table index hasn't been resolved yet");
            let stats = self.statistics.per_kind_index.get(index as usize)
                .expect("Could not find entry per index");
            match self.statistics.per_kind_name.entry(key.kind.clone()) {
                std::collections::hash_map::Entry::Occupied(mut entry) => {
                    let borrow = entry.get_mut();
                    *borrow += stats.clone();
                }
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(stats.clone());
                }
            }
            self.statistics.per_description.insert(key, stats.clone());
        }
        self.statistics.number_of_files = 1;
        self.statistics.compressed_bytes = self.data.len();
        self.statistics.uncompressed_bytes += self.statistics.grammar_table.compression.before_bytes
            + self.statistics.strings_table.compression.before_bytes
            + self.statistics.tree.compression.before_bytes;
        Ok((self.data.clone().into_boxed_slice(), self.statistics))
    }
}

impl TokenWriter for TreeTokenWriter {
    type Tree = Tree;
    type Error = TokenWriterError;
    type Data = Box<[u8]>;
    type Statistics = Statistics;

    fn done(self) -> Result<(Self::Data, Self::Statistics), Self::Error> {
        (self as TreeTokenWriter).done()
    }

    fn float(&mut self, value: Option<f64>) -> Result<Self::Tree, Self::Error> {
        let bytes : Vec<_> = bytes::float::bytes_of_float(value).iter().cloned().collect();
        debug!(target: "multipart", "writing float {:?} => {:?}", value, bytes);
        Ok(self.register(LabelledItem {
            item: Item::Encoded(bytes),
            nature: Nature::Float,
        }))
    }

    fn bool(&mut self, data: Option<bool>)  -> Result<Self::Tree, Self::Error> {
        let bytes = bytes::bool::bytes_of_bool(data).iter().cloned().collect();
        debug!(target: "multipart", "writing bool {:?} => {:?}", data, bytes);
        Ok(self.register(LabelledItem {
            item: Item::Encoded(bytes),
            nature: Nature::Bool
        }))
    }

    fn string(&mut self, data: Option<&str>) -> Result<Self::Tree, Self::Error> {
        let key = data.map(str::to_string);
        let index = self.strings_table
            .get(&key)
            .map(|entry| entry.index.clone());

        if let Some(index) = index {
            return Ok(self.register(LabelledItem {
                item: Item::String(index),
                nature: Nature::String
            }));
        }
        let index = self.strings_table.insert(key);
        debug!(target: "multipart", "writing string {:?} => {:?}", data, index);
        Ok(self.register(LabelledItem {
            item: Item::String(index),
            nature: Nature::String
        }))
    }
    fn list(&mut self, mut children: Vec<Self::Tree>) -> Result<Self::Tree, Self::Error> {
        let mut items : Vec<Rc<LabelledItem>> = Vec::with_capacity(children.len() + 1);
        // First child is the number of children.
        let mut encoded_number_of_items = Vec::with_capacity(8);
        encoded_number_of_items.write_varnum(children.len() as u32)
            .map_err(TokenWriterError::WriteError)?;
        items.push(Rc::new(LabelledItem {
            item: Item::Encoded(encoded_number_of_items),
            nature: Nature::ListHeader,
        }));

        let len = children.len();
        // Next, we have `children`.
        let children : Vec<_> = children.drain(..)
            .map(|tree| tree.0.clone())
            .collect();
        items.extend(children);
        debug!(target: "multipart", "writing list with {} => {} items", len, items.len());
        Ok(self.register(LabelledItem {
            item: Item::List(items),
            nature: Nature::ListWhole,
        }))
    }
    fn untagged_tuple(&mut self, children: &[Self::Tree]) -> Result<Self::Tree, Self::Error> {
        let result = LabelledItem {
            item: Item::List(children.iter()
                .map(|tree| tree.0.clone())
                .collect()
            ),
            nature: Nature::UntaggedTuple,
        };
        debug!(target: "multipart", "writing untagged tuple {} children", children.len());
        Ok(self.register(result))
    }

    // Tagged tuple:
    //
    // All tagged tuples with the same `tag` are written with the fields in the same order.
    //
    // - index in the grammar table (varnum);
    // - for each item, in the order specified
    //    - the item (see item)
    fn tagged_tuple(&mut self, name: &str, children: &[(&str, Self::Tree)]) -> Result<Self::Tree, Self::Error> {
        let mut data : Vec<Rc<LabelledItem>> = Vec::with_capacity(children.len() + 1);
        {
            let description = NodeDescription {
                kind: name.to_string(),
                fields: children.iter()
                    .map(|&(s, _)| s.to_string())
                    .collect() // FIXME: We shouldn't build the Vec unless we know we need it.
            };
            debug!(target: "multipart", "writing tagged tuple {} with {} children as {:?}",
                name,
                children.len(),
                description,
            );
            let index = self.grammar_table.insert(description);
            debug!(target: "multipart", "tagged tuple index: {:?}", index);

            // Now add the prefix
            data.push(Rc::new(LabelledItem {
                item: Item::NodeDescription(index),
                nature: Nature::TaggedTupleHeader,
            }));
            for &(_, ref child) in children {
                data.push(child.0.clone())
            }
        }
        Ok(self.register(LabelledItem {
            item: Item::List(data),
            nature: Nature::TaggedTupleWhole,
        }))
    }
}


pub struct TreeTokenWriter {
    /// The table defining the accepted TaggedTuple
    /// and how they are laid out in the binary.
    grammar_table: WriterTable<NodeDescription>,

    /// The strings used in the binary.
    strings_table: WriterTable<Option<String>>,

    root: Option<Tree>,

    data: Vec<u8>,

    options: WriteOptions,

    statistics: Statistics,
}


#[derive(Clone, Debug)]
pub struct SectionStatistics {
    /// Number of entries in this table.
    pub entries: usize,

    /// Used when collating results across several files:
    /// max number of entries in a single file.
    pub max_entries: usize,

    pub compression: CompressionResult,
}

impl Default for SectionStatistics {
    fn default() -> Self {
        SectionStatistics {
            entries: 0,
            max_entries: 0,
            compression: CompressionResult {
                before_bytes: 0,
                after_bytes: 0,
                algorithms: HashSet::new(),
            }
        }
    }
}

impl AddAssign for CompressionResult {
    fn add_assign(&mut self, mut rhs: Self) {
        self.before_bytes += rhs.before_bytes;
        self.after_bytes += rhs.after_bytes;
        self.algorithms.extend(rhs.algorithms.drain());
    }
}

impl AddAssign for SectionStatistics {
    fn add_assign(&mut self, rhs: Self) {
        self.entries += rhs.entries;
        if rhs.max_entries > self.max_entries {
            self.max_entries = rhs.max_entries;
        }
        self.compression += rhs.compression;
    }
}

#[derive(Clone, Debug, Default)]
pub struct NodeStatistics {
    /// Total number of entries of this node.
    pub entries: usize,

    /// Used when collating results across several files:
    /// max number of entries in a single file.
    pub max_entries: usize,

    /// Number of bytes used to represent the node, minus subnodes
    /// (e.g. the length of the entry in the grammar table).
    pub own_bytes: usize,

    /// Number of bytes used to represent the node, including primitive
    /// subnodes (e.g. anything but lists) but not compound subnodes.
    pub shallow_bytes: usize,

    /// Number of bytes used to represent the node, including all subnodes.
    pub total_bytes: usize,
}

impl AddAssign for NodeStatistics {
    fn add_assign(&mut self, rhs: Self) {
        self.entries += rhs.entries;
        if rhs.max_entries > self.max_entries {
            self.max_entries = rhs.max_entries;
        }
        self.own_bytes += rhs.own_bytes;
        self.shallow_bytes += rhs.shallow_bytes;
        self.total_bytes += rhs.total_bytes;
    }
}

#[derive(Clone, Debug, Default)]
pub struct Statistics {
    pub grammar_table: SectionStatistics,
    pub strings_table: SectionStatistics,
    pub tree: SectionStatistics,

    pub per_kind_index: VecMap<NodeStatistics>,
    pub per_kind_name: HashMap<String, NodeStatistics>,
    pub per_description: HashMap<NodeDescription, NodeStatistics>,

    /// Mapping length -> number of lists of that length.
    pub list_lengths: VecMap<usize>,

    /// Mapping length -> number of strings of that length.
    pub string_lengths: VecMap<usize>,

    /// Mapping number of occurrences -> number of lists of that length.
    pub string_usage: VecMap<usize>,

    pub bool: NodeStatistics,
    pub float: NodeStatistics,
    pub string: NodeStatistics,
    pub list: NodeStatistics,
    pub list_header: NodeStatistics,
    pub tagged_header: NodeStatistics,
    pub tagged_tuple: NodeStatistics,

    pub number_of_files: usize,
    pub uncompressed_bytes: usize,
    pub compressed_bytes: usize,
    pub source_bytes: Option<usize>,
}
impl AddAssign for Statistics {
    fn add_assign(&mut self, rhs: Self) {
        let copy = self.clone() + rhs; // We don't attempt to optimize.
        *self = copy;
    }
}
impl Add for Statistics {
    type Output = Self;
    fn add(mut self, mut rhs: Self) -> Self {
        self.grammar_table += rhs.grammar_table;
        self.strings_table += rhs.strings_table;
        self.tree += rhs.tree;

        for (key, value) in rhs.per_kind_index.drain() {
            use vec_map::Entry::*;
            match self.per_kind_index.entry(key) {
                Occupied(mut entry) => {
                    *entry.get_mut() += value;
                }
                Vacant(entry) => {
                    entry.insert(value);
                }
            }
        }
        for (key, value) in rhs.per_description.drain() {
            use std::collections::hash_map::Entry::*;
            match self.per_description.entry(key) {
                Occupied(mut entry) => {
                    *entry.get_mut() += value;
                }
                Vacant(entry) => {
                    entry.insert(value);
                }
            }
        }
        for (key, value) in rhs.per_kind_name.drain() {
            use std::collections::hash_map::Entry::*;
            match self.per_kind_name.entry(key) {
                Occupied(mut entry) => {
                    *entry.get_mut() += value;
                }
                Vacant(entry) => {
                    entry.insert(value);
                }
            }
        }
        for (key, value) in rhs.list_lengths.drain() {
            use vec_map::Entry::*;
            match self.list_lengths.entry(key) {
                Occupied(mut entry) => {
                    *entry.get_mut() += value;
                }
                Vacant(entry) => {
                    entry.insert(value);
                }
            }
        }
        for (key, value) in rhs.string_lengths.drain() {
            use vec_map::Entry::*;
            match self.string_lengths.entry(key) {
                Occupied(mut entry) => {
                    *entry.get_mut() += value;
                }
                Vacant(entry) => {
                    entry.insert(value);
                }
            }
        }
        for (key, value) in rhs.string_usage.drain() {
            use vec_map::Entry::*;
            match self.string_usage.entry(key) {
                Occupied(mut entry) => {
                    *entry.get_mut() += value;
                }
                Vacant(entry) => {
                    entry.insert(value);
                }
            }
        }

        self.bool += rhs.bool;
        self.float += rhs.float;
        self.string += rhs.string;
        self.list += rhs.list;
        self.list_header += rhs.list_header;
        self.tagged_header += rhs.tagged_header;
        self.tagged_tuple += rhs.tagged_tuple;

        self.number_of_files += rhs.number_of_files;
        self.compressed_bytes += rhs.compressed_bytes;
        self.uncompressed_bytes += rhs.uncompressed_bytes;
        self.source_bytes = match (self.source_bytes, rhs.source_bytes) {
            (Some(x), Some(y)) => Some(x + y),
            _ => None
        };

        self
    }
}

impl Statistics {
    pub fn with_source_bytes(mut self, source_bytes: usize) -> Self {
        self.source_bytes = Some(source_bytes);
        self
    }
}

// Shortcuts to display statistics

struct SectionAndStatistics<'a> {
    total_compressed_bytes: usize,
    total_uncompressed_bytes: usize,
    section: &'a SectionStatistics,
}
impl<'a> Display for SectionAndStatistics<'a> {
    fn fmt(&self, f: &mut Formatter) -> Result<(), std::fmt::Error> {
        write!(f, "\t\t\tEntries: {}\n", self.section.entries)?;
        if self.section.max_entries != self.section.entries {
            write!(f, "\t\t\tMax entries per file: {}\n", self.section.max_entries)?;
        }
        write!(f, "\t\t\tCompression: [")?;
        let mut first = true;
        for item in &self.section.compression.algorithms {
            if !first {
                write!(f, ", ")?;
            }
            first = false;
            write!(f, "{}", item.name())?;
        }
        write!(f, "]\n")?;
        write!(f, "\t\t\tUncompressed bytes: {} ({:.2}%)\n", self.section.compression.before_bytes, 100. * (self.section.compression.before_bytes as f64) / (self.total_uncompressed_bytes as f64))?;
        write!(f, "\t\t\tCompressed bytes: {} ({:.2}%)\n", self.section.compression.after_bytes, 100. * (self.section.compression.after_bytes as f64) / (self.total_compressed_bytes as f64))?;
        Ok(())
    }
}

struct NodeAndStatistics<'a> {
    name: &'a str,
    stats: &'a NodeStatistics,
    header_bytes: usize,
    total_uncompressed_bytes: usize,
    total_number_of_entries: usize,
}
impl<'a> Display for NodeAndStatistics<'a> {
    fn fmt(&self, f: &mut Formatter) -> Result<(), std::fmt::Error> {
        write!(f, "\t\t{}:\n", self.name)?;
        write!(f, "\t\t\tEntries: {} ({:.2}%)\n", self.stats.entries, 100. * (self.stats.entries as f64) / (self.total_number_of_entries as f64))?;
        if self.stats.max_entries != self.stats.entries {
            write!(f, "\t\t\tMax entries per file: {}\n", self.stats.max_entries)?;
        }
        write!(f, "\t\t\tOwn bytes: {} ({:.2}%)\n", self.stats.own_bytes, 100. * (self.stats.own_bytes as f64) / (self.total_uncompressed_bytes as f64))?;
        if self.header_bytes != 0 {
            write!(f, "\t\t\tHeader bytes: {} ({:.2}%)\n", self.header_bytes, 100. * (self.header_bytes as f64) / (self.total_uncompressed_bytes as f64))?;
        }
        if self.stats.shallow_bytes != self.stats.own_bytes && self.stats.shallow_bytes != 0 {
            write!(f, "\t\t\tShallow bytes: {} ({:.2}%)\n", self.stats.shallow_bytes, 100. * (self.stats.shallow_bytes as f64) / (self.total_uncompressed_bytes as f64))?;
        }
        if self.stats.total_bytes != self.stats.own_bytes {
            write!(f, "\t\t\tTotal bytes: {} ({:.2}%)\n", self.stats.total_bytes, 100. * (self.stats.total_bytes as f64) / (self.total_uncompressed_bytes as f64))?;
        }
        Ok(())
    }
}

struct NodeNameAndStatistics {
    total_uncompressed_bytes: usize,
    nodes: Vec<(String, NodeStatistics)>
}

impl Display for NodeNameAndStatistics {
    fn fmt(&self, f: &mut Formatter) -> Result<(), std::fmt::Error> {
        let total_number_of_entries : usize = self.nodes
            .iter()
            .map(|node| node.1.entries)
            .sum();
        // FIXME: Ugly. Find a better way to handle indentation.
        for &(ref name, ref stats) in &self.nodes {
            let for_display = NodeAndStatistics {
                header_bytes: 0,
                total_uncompressed_bytes: self.total_uncompressed_bytes,
                total_number_of_entries,
                name: name,
                stats
            };
            write!(f, "{}", for_display)?;
        }
        Ok(())
    }
}

struct NodeDescriptionAndStatistics {
    total_uncompressed_bytes: usize,
    nodes: Vec<(NodeDescription, NodeStatistics)>
}

impl Display for NodeDescriptionAndStatistics {
    fn fmt(&self, f: &mut Formatter) -> Result<(), std::fmt::Error> {
        let total_number_of_entries : usize = self.nodes
            .iter()
            .map(|node| node.1.entries)
            .sum();
        // FIXME: Ugly. Find a better way to handle indentation.
        for &(ref description, ref stats) in &self.nodes {
            write!(f, "\t\t{} [", description.kind.to_string())?;
            let mut start = true;
            for field in &description.fields {
                if !start {
                    write!(f, ", ")?;
                }
                start = false;
                write!(f, "{}", field.to_string())?;
            }
            write!(f, "]\n")?;
            write!(f, "\t\t\tEntries: {} ({:.2}%)\n", stats.entries, 100. * (stats.entries as f64) / (total_number_of_entries as f64))?;
            write!(f, "\t\t\tOwn bytes: {} ({:.2}%)\n", stats.own_bytes, 100. * (stats.own_bytes as f64) / (self.total_uncompressed_bytes as f64))?;
            write!(f, "\t\t\tShallow bytes: {} ({:.2}%)\n", stats.shallow_bytes, 100. * (stats.shallow_bytes as f64) / (self.total_uncompressed_bytes as f64))?;
            write!(f, "\t\t\tTotal bytes: {} ({:.2}%)\n", stats.total_bytes, 100. * (stats.total_bytes as f64) / (self.total_uncompressed_bytes as f64))?;
        }
        Ok(())
    }
}

struct ListLengthsAndNumber(Vec<(usize, usize)>, String);

impl Display for ListLengthsAndNumber {
    fn fmt(&self, f: &mut Formatter) -> Result<(), std::fmt::Error> {
        // FIXME: Ugly. Find a better way to handle indentation.
        let total_number_of_entries : usize = self.0
            .iter()
            .map(|&(_, number)| number)
            .sum();
        for &(ref length, ref number) in &self.0 {
            write!(f, "\t\t{} {} x {} ({:.2}%)\n", self.1, length, number, 100. * (*number as f64) / (total_number_of_entries as f64))?;
        }
        Ok(())
    }
}

impl Display for Statistics {
    fn fmt(&self, f: &mut Formatter) -> Result<(), std::fmt::Error> {
        // Sort entries by number of uses.
        let mut per_kind : Vec<_> = self.per_kind_name.iter()
            .map(|(a, b)| (a.clone(), b.clone()))
            .collect();
        per_kind.sort_unstable_by(|a, b| usize::cmp(&b.1.entries, &a.1.entries));

        // Per kind expanded.
        let mut per_description : Vec<_> = self.per_description.iter()
            .map(|(a, b)| (a.clone(), b.clone()))
            .collect();
        per_description.sort_unstable_by(|a, b| usize::cmp(&b.1.entries, &a.1.entries));

        let mut list_per_size : Vec<_> = self.list_lengths.iter()
            .map(|(a, b)| (a.clone(), b.clone()))
            .collect();
        list_per_size.sort_unstable_by(|a, b| usize::cmp(&b.1, &a.1));

        let mut strings_per_size : Vec<_> = self.string_lengths.iter()
            .map(|(a, b)| (a.clone(), b.clone()))
            .collect();
        strings_per_size.sort_unstable_by(|a, b| usize::cmp(&b.1, &a.1));

        let mut strings_per_usage : Vec<_> = self.string_usage.iter()
            .map(|(a, b)| (a.clone(), b.clone()))
            .collect();
        strings_per_usage.sort_unstable_by(|a, b| usize::cmp(&b.1, &a.1));

        let total_number_of_tokens = self.bool.entries
            + self.float.entries
            + self.string.entries
            + self.list.entries
            + self.tagged_tuple.entries;
        write!(f, "
Statistics
\tFiles:
\t\tNumber: {number_of_files}
\t\tTotal source bytes: {total_source_bytes}
\t\tTotal uncompressed bytes: {total_uncompressed_bytes}
\t\tTotal compressed bytes: {total_compressed_bytes}
\t\tRatio: {compression_ratio}
\tSections:
\t\tGrammar:
{section_grammar}
\t\tStrings:
{section_strings}
\t\tTree:
{section_tree}
\tNodes (grammar entries collapsed):
{collapsed_nodes}
\tNodes (grammar entries expanded):
{expanded_nodes}
\tTokens:
{token_bool}
{token_float}
{token_string}
{token_list}
{token_tagged_tuple}
\tLists per size:
{lists_per_size}
\tStrings per size:
{strings_per_size}
\tStrings per usage:
{strings_per_usage}
",
        number_of_files = self.number_of_files,
        total_source_bytes = match self.source_bytes {
            None => "<not available>".to_string(),
            Some(ref bytes) => format!("{}", bytes)
        },
        total_uncompressed_bytes = self.uncompressed_bytes,
        total_compressed_bytes = self.compressed_bytes,
        compression_ratio = match self.source_bytes {
            None => "<not available>".to_string(),
            Some(ref bytes) => format!("{:.2}", (self.compressed_bytes as f64) / (*bytes as f64))
        },
        lists_per_size = ListLengthsAndNumber(list_per_size, "length".to_string()),
        strings_per_size = ListLengthsAndNumber(strings_per_size, "length".to_string()),
        strings_per_usage = ListLengthsAndNumber(strings_per_usage, "occurrences".to_string()),
        section_grammar = SectionAndStatistics {
            total_uncompressed_bytes: self.uncompressed_bytes,
            total_compressed_bytes: self.compressed_bytes,
            section: &self.grammar_table,
        },
        section_strings = SectionAndStatistics {
            total_uncompressed_bytes: self.uncompressed_bytes,
            total_compressed_bytes: self.compressed_bytes,
            section: &self.strings_table,
        },
        section_tree = SectionAndStatistics {
            total_uncompressed_bytes: self.uncompressed_bytes,
            total_compressed_bytes: self.compressed_bytes,
            section: &self.tree,
        },
        collapsed_nodes = NodeNameAndStatistics {
            total_uncompressed_bytes: self.uncompressed_bytes,
            nodes: per_kind
        },
        expanded_nodes = NodeDescriptionAndStatistics {
            total_uncompressed_bytes: self.uncompressed_bytes,
            nodes: per_description
        },
        token_bool = NodeAndStatistics {
            name: "Bool",
            stats: &self.bool,
            total_number_of_entries: total_number_of_tokens,
            total_uncompressed_bytes: self.uncompressed_bytes,
            header_bytes: 0,
        },
        token_float = NodeAndStatistics {
            name: "Float",
            stats: &self.float,
            total_number_of_entries: total_number_of_tokens,
            total_uncompressed_bytes: self.uncompressed_bytes,
            header_bytes: 0,
        },
        token_string = NodeAndStatistics {
            name: "String indices",
            stats: &self.string,
            total_number_of_entries: total_number_of_tokens,
            total_uncompressed_bytes: self.uncompressed_bytes,
            header_bytes: 0,
        },
        token_list = NodeAndStatistics {
            name: "List",
            stats: &self.list,
            total_number_of_entries: total_number_of_tokens,
            total_uncompressed_bytes: self.uncompressed_bytes,
            header_bytes: self.list_header.own_bytes,
        },
        token_tagged_tuple = NodeAndStatistics {
            name: "Tagged Tuple",
            stats: &self.tagged_tuple,
            total_number_of_entries: total_number_of_tokens,
            total_uncompressed_bytes: self.uncompressed_bytes,
            header_bytes: self.tagged_header.own_bytes,
        },
        )
    }
}
