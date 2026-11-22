//! Unity asset reader (`dm/gfx/tools/unityfs.py`): UnityFS bundles, serialized files, objects decoded by their type trees, a Shader's platform subprograms.
//!
//! The Python decodes whole files; here a serialized file is read through a [`Source`]: its metadata (the type table and the object table, everything before the data offset)
//! and then only the objects of the classes the caller asks for (Shader 48, ComputeShader 72, TextAsset 49).

use crate::bytes::*;
use crate::error::{Failure, Res};
use crate::red;
use crate::source::Source;
use crate::unity_trees::KNOWN_TREES;
use std::collections::HashMap;
use std::sync::Arc;

pub const CLASS_SHADER: i32 = 48;
pub const CLASS_COMPUTE_SHADER: i32 = 72;
pub const CLASS_TEXT_ASSET: i32 = 49;

/// A type tree node (`unityfs.Node`): the byte sizes are not kept
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub ty: String,
    pub name: String,
    pub flags: i32,
    pub kids: Vec<Node>,
}

impl Node {
    fn new(ty: &str, name: &str, flags: i32) -> Node {
        Node {
            ty: ty.to_string(),
            name: name.to_string(),
            flags,
            kids: Vec::new(),
        }
    }
}

/// `{(class ID, type hash): type tree or None}`
pub type Trees = HashMap<(i32, [u8; 16]), Option<Arc<Node>>>;

fn lz4(raw: &[u8], size: usize) -> Res<Vec<u8>> {
    let mut out = vec![0u8; size];
    match lz4_flex::block::decompress_into(raw, &mut out) {
        Ok(n) => {
            out.truncate(n);
            Ok(out)
        }
        Err(e) => red!(
            "LZ4BlockError: Decompression failed: corrupt input or insufficient space in destination buffer ({e})"
        ),
    }
}

fn decomp(raw: &[u8], size: usize, kind: u32) -> Res<Vec<u8>> {
    match kind {
        0 => Ok(raw.to_vec()),
        1 => {
            // Unity's LZMA: 5 property bytes + raw stream
            if raw.len() < 5 {
                red!("LZMA: the stream is shorter than its properties");
            }
            let mut hdr = Vec::with_capacity(raw.len() + 8);
            hdr.extend_from_slice(&raw[..5]);
            hdr.extend_from_slice(&(size as u64).to_le_bytes());
            hdr.extend_from_slice(&raw[5..]);
            let mut out = Vec::with_capacity(size);
            let mut inp = std::io::Cursor::new(hdr);
            if let Err(e) = lzma_rs::lzma_decompress(&mut inp, &mut out) {
                red!("LZMAError: {e}");
            }
            out.truncate(size);
            Ok(out)
        }
        2 | 3 => lz4(raw, size),
        k => red!("UnityFS: unknown compression {k}"),
    }
}

fn cstr(d: &[u8], p: usize) -> Res<(String, usize)> {
    match find(d, &[0], p) {
        Some(e) => Ok((String::from_utf8_lossy(&d[p..e]).into_owned(), e + 1)),
        None => red!("ValueError: subsection not found"),
    }
}

/// `bundle_files(d)`: a UnityFS bundle to `[(node path, bytes)]`
pub fn bundle_files(d: &[u8]) -> Res<Vec<(String, Vec<u8>)>> {
    if !starts(d, b"UnityFS\0") {
        red!("ValueError: not UnityFS");
    }
    let fmt = u32be(d, 8)?;
    let (_, p) = cstr(d, 12)?;
    let (_, mut p) = cstr(d, p)?;
    let (_size, csz, usz, flags) = (
        i64be(d, p)?,
        u32be(d, p + 8)? as usize,
        u32be(d, p + 12)? as usize,
        u32be(d, p + 16)?,
    );
    p += 20;
    if fmt >= 7 {
        p = (p + 15) & !15;
    }
    let info_raw = if flags & 0x80 != 0 {
        sl(d, d.len().saturating_sub(csz), d.len()).to_vec()
    } else {
        let r = at(d, p, csz).to_vec();
        p += csz;
        r
    };
    let info = decomp(&info_raw, usz, flags & 0x3f)?;
    let mut q = 16usize;
    let nb = i32be(&info, q)?;
    q += 4;
    let mut blocks = Vec::new();
    for _ in 0..nb.max(0) {
        let (u, c, f) = (
            u32be(&info, q)? as usize,
            u32be(&info, q + 4)? as usize,
            u16be(&info, q + 8)? as u32,
        );
        q += 10;
        blocks.push((u, c, f));
    }
    let nn = i32be(&info, q)?;
    q += 4;
    let mut nodes = Vec::new();
    for _ in 0..nn.max(0) {
        let (off, sz, _f) = (
            i64be(&info, q)?,
            i64be(&info, q + 8)?,
            u32be(&info, q + 16)?,
        );
        q += 20;
        let (name, nq) = cstr(&info, q)?;
        q = nq;
        nodes.push((off, sz, name));
    }
    if flags & 0x200 != 0 {
        p = (p + 15) & !15; // BlockInfoNeedPaddingAtStart
    }
    let mut out: Vec<u8> = Vec::new();
    for (u, c, f) in blocks {
        out.extend_from_slice(&decomp(at(d, p, c), u, f & 0x3f)?);
        p += c;
    }
    Ok(nodes
        .into_iter()
        .map(|(off, sz, name)| {
            let a = (off.max(0) as usize).min(out.len());
            let b = ((off + sz).max(0) as usize).min(out.len()).max(a);
            (name, out[a..b].to_vec())
        })
        .collect())
}

/// `common_strings(image)`: Unity's common string table in the game's main, `{offset: string}`; None when main has none (not a Unity game)
pub fn common_strings(image: &[u8]) -> Option<HashMap<u32, String>> {
    let a = find(image, b"AABB\0AnimationClip\0AnimationCurve\0", 0)?;
    let mut tab = HashMap::new();
    let mut p = a;
    loop {
        let e = find(image, &[0], p)?; // `image.index` raises ValueError: no table
        if e == p {
            break;
        }
        let s = &image[p..e];
        if !s.iter().all(|&ch| (0x20..0x7f).contains(&ch)) {
            break;
        }
        tab.insert((p - a) as u32, String::from_utf8_lossy(s).into_owned());
        p = e + 1;
    }
    Some(tab)
}

// ---- decoded objects --------------------------------------------------------------------------------------------------------

/// A decoded object field (the Python's int / float / bool / str / bytes / list / dict)
#[derive(Debug, Clone)]
pub enum Val<'a> {
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(&'a str),
    Bytes(&'a [u8]),
    List(Vec<Val<'a>>),
    Obj(Vec<(&'a str, Val<'a>)>),
}

impl<'a> Val<'a> {
    /// `d[key]` of a dict (the last of equal names, as a dict comprehension keeps it)
    pub fn get(&self, key: &str) -> Option<&Val<'a>> {
        match self {
            Val::Obj(kv) => kv.iter().rev().find(|(k, _)| *k == key).map(|(_, v)| v),
            _ => None,
        }
    }
    pub fn list(&self) -> &[Val<'a>] {
        match self {
            Val::List(v) => v,
            _ => &[],
        }
    }
    pub fn int(&self) -> Option<i64> {
        match self {
            Val::Int(i) => Some(*i),
            Val::Bool(b) => Some(*b as i64),
            _ => None,
        }
    }
    pub fn bytes(&self) -> &'a [u8] {
        match self {
            Val::Bytes(b) => b,
            Val::Str(s) => s.as_bytes(),
            _ => &[],
        }
    }
    pub fn str(&self) -> String {
        match self {
            Val::Str(s) => s.to_string(),
            Val::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
            Val::Int(i) => i.to_string(),
            _ => String::new(),
        }
    }
    /// `bool(v)` of an object that may be absent
    pub fn truthy(&self) -> bool {
        match self {
            Val::Int(i) => *i != 0,
            Val::Float(f) => *f != 0.0,
            Val::Bool(b) => *b,
            Val::Str(s) => !s.is_empty(),
            Val::Bytes(b) => !b.is_empty(),
            Val::List(v) => !v.is_empty(),
            Val::Obj(v) => !v.is_empty(),
        }
    }
}

#[derive(Clone, Copy)]
enum Prim {
    I8,
    U8,
    Bool,
    I16,
    U16,
    I32,
    U32,
    I64,
    U64,
    F32,
    F64,
}

fn prim(ty: &str) -> Option<Prim> {
    Some(match ty {
        "SInt8" => Prim::I8,
        "UInt8" | "char" => Prim::U8,
        "bool" => Prim::Bool,
        "SInt16" | "short" => Prim::I16,
        "UInt16" | "unsigned short" => Prim::U16,
        "SInt32" | "int" | "Type*" => Prim::I32,
        "UInt32" | "unsigned int" => Prim::U32,
        "SInt64" | "long long" => Prim::I64,
        "UInt64" | "unsigned long long" | "FileSize" => Prim::U64,
        "float" => Prim::F32,
        "double" => Prim::F64,
        _ => return None,
    })
}

impl Prim {
    fn size(self) -> usize {
        match self {
            Prim::I8 | Prim::U8 | Prim::Bool => 1,
            Prim::I16 | Prim::U16 => 2,
            Prim::I32 | Prim::U32 | Prim::F32 => 4,
            Prim::I64 | Prim::U64 | Prim::F64 => 8,
        }
    }
}

/// `Reader`: a reader over the object's bytes in the file's byte order
struct Rd<'a> {
    d: &'a [u8],
    p: usize,
    le: bool,
}

impl<'a> Rd<'a> {
    fn raw(&mut self, n: usize) -> Res<&'a [u8]> {
        let s = match self.p.checked_add(n).and_then(|e| self.d.get(self.p..e)) {
            Some(s) => s,
            None => return Err(short("Reader")),
        };
        self.p += n;
        Ok(s)
    }
    fn u8(&mut self) -> Res<u8> {
        Ok(self.raw(1)?[0])
    }
    fn i16(&mut self) -> Res<i16> {
        let b: [u8; 2] = self.raw(2)?.try_into().unwrap();
        Ok(if self.le {
            i16::from_le_bytes(b)
        } else {
            i16::from_be_bytes(b)
        })
    }
    fn u16(&mut self) -> Res<u16> {
        Ok(self.i16()? as u16)
    }
    fn i32(&mut self) -> Res<i32> {
        let b: [u8; 4] = self.raw(4)?.try_into().unwrap();
        Ok(if self.le {
            i32::from_le_bytes(b)
        } else {
            i32::from_be_bytes(b)
        })
    }
    fn u32(&mut self) -> Res<u32> {
        Ok(self.i32()? as u32)
    }
    fn i64(&mut self) -> Res<i64> {
        let b: [u8; 8] = self.raw(8)?.try_into().unwrap();
        Ok(if self.le {
            i64::from_le_bytes(b)
        } else {
            i64::from_be_bytes(b)
        })
    }
    fn u64(&mut self) -> Res<u64> {
        Ok(self.i64()? as u64)
    }
    fn align(&mut self) {
        self.p = (self.p + 3) & !3;
    }
    fn prim(&mut self, p: Prim) -> Res<Val<'a>> {
        Ok(match p {
            Prim::I8 => Val::Int(self.u8()? as i8 as i64),
            Prim::U8 => Val::Int(self.u8()? as i64),
            Prim::Bool => Val::Bool(self.u8()? != 0),
            Prim::I16 => Val::Int(self.i16()? as i64),
            Prim::U16 => Val::Int(self.u16()? as i64),
            Prim::I32 => Val::Int(self.i32()? as i64),
            Prim::U32 => Val::Int(self.u32()? as i64),
            Prim::I64 => Val::Int(self.i64()?),
            Prim::U64 => Val::Int(self.u64()? as i64),
            Prim::F32 => Val::Float(f32::from_bits(self.u32()?) as f64),
            Prim::F64 => Val::Float(f64::from_bits(self.u64()?)),
        })
    }
}

/// Values one object may decode to: `NODES_PER_BYTE` for each byte of the object, plus `NODES_BASE` (review r4 4.4). Every value that reads its own bytes reads at least
/// one (a primitive) or four (a count, a length), so only structures can come for free: an element type with no field reads nothing, and one whose only field is an array of such
/// elements reads 4 bytes per element and then up to the whole object's size in empty values, which is the file's size squared (3,804 bytes made 1,805,002 values, `Ok`). One budget for the
/// whole decode bounds every nesting of that, where a bound per array bounds one level only. An honest object needs far less: the most values per byte any element of the shipped
/// Shader / ComputeShader layouts decodes to is 1.5 (`tests::honest_objects_stay_far_under_the_budget`), and the objects the split decodes are dominated by byte blobs (one value each).
const NODES_PER_BYTE: usize = 8;
const NODES_BASE: usize = 4096;

/// the decode budget of an object of `len` bytes
fn node_budget(len: usize) -> usize {
    len.saturating_mul(NODES_PER_BYTE)
        .saturating_add(NODES_BASE)
}

/// `n` more values from the budget `left` of an object of `len` bytes
fn spend(left: &mut usize, n: usize, len: usize) -> Res<()> {
    match left.checked_sub(n) {
        Some(l) => {
            *left = l;
            Ok(())
        }
        None => red!(
            "MemoryError: an object of {len} bytes that decodes to more than {} values (structures that read no bytes, nested in arrays)",
            node_budget(len)
        ),
    }
}

/// `_read(Reader(data), root)` with the decode budget of `data`
fn read_object<'a>(r: &mut Rd<'a>, root: &'a Node) -> Res<Val<'a>> {
    let mut left = node_budget(r.d.len());
    read_node(r, root, &mut left)
}

/// `_read(r, n)`: one value of the type tree node n; `left` is what is left of the object's decode budget
fn read_node<'a>(r: &mut Rd<'a>, n: &'a Node, left: &mut usize) -> Res<Val<'a>> {
    spend(left, 1, r.d.len())?;
    let v;
    if let (Some(p), true) = (prim(&n.ty), n.kids.is_empty()) {
        v = r.prim(p)?;
    } else if n.ty == "string" {
        let ln = r.i32()?;
        if ln < 0 {
            red!("negative string length {ln}");
        }
        let b = sl(r.d, r.p, r.p + ln as usize);
        r.p += ln as usize;
        v = match std::str::from_utf8(b) {
            Ok(s) => Val::Str(s),
            Err(_) => Val::Bytes(b),
        };
        if n.kids.first().is_some_and(|k| k.flags & 0x4000 != 0) {
            r.align();
        }
    } else if n.kids.first().is_some_and(|k| k.ty == "Array") {
        let arr = &n.kids[0];
        let cnt = r.i32()?;
        if cnt < 0 {
            red!("negative array count {cnt}");
        }
        let cnt = cnt as usize;
        let el = match arr.kids.get(1) {
            Some(e) => e,
            None => red!("IndexError: an Array node without an element type"),
        };
        if matches!(el.ty.as_str(), "UInt8" | "char" | "SInt8") && el.kids.is_empty() {
            let b = sl(r.d, r.p, r.p.saturating_add(cnt));
            r.p = r.p.saturating_add(cnt);
            v = Val::Bytes(b);
        } else if let (Some(p), true) = (prim(&el.ty), el.kids.is_empty()) {
            let w = p.size();
            // struct.unpack_from needs every element
            if cnt
                .checked_mul(w)
                .and_then(|t| r.p.checked_add(t))
                .is_none_or(|e| e > r.d.len())
            {
                return Err(short("array of primitives"));
            }
            spend(left, cnt, r.d.len())?;
            let mut list = Vec::with_capacity(cnt);
            for _ in 0..cnt {
                list.push(r.prim(p)?);
            }
            v = Val::List(list);
        } else {
            // The count is the file's word. An element of a type that reads nothing (a struct without fields) makes no progress, so nothing would stop
            // 2^31 of them (the Python's list comprehension runs out of memory). Such an element reads the same thing every time, and a count larger than the
            // bytes left in the object is no real array: it is refused. Any element that reads a byte runs out of data on its own (a short read).
            let room = r.d.len().saturating_sub(r.p);
            let mut list = Vec::new();
            for _ in 0..cnt {
                let before = r.p;
                list.push(read_node(r, el, left)?);
                if r.p == before && cnt > room {
                    red!(
                        "MemoryError: an array of {cnt} elements that read no bytes, in an object with {room} bytes left"
                    );
                }
            }
            v = Val::List(list);
        }
        if arr.flags & 0x4000 != 0 {
            r.align();
        }
    } else if n.ty == "TypelessData" {
        let cnt = r.i32()?;
        if cnt < 0 {
            red!("negative data length {cnt}");
        }
        let b = sl(r.d, r.p, r.p + cnt as usize);
        r.p += cnt as usize;
        v = Val::Bytes(b);
    } else {
        let mut kv = Vec::with_capacity(n.kids.len());
        for k in &n.kids {
            kv.push((k.name.as_str(), read_node(r, k, left)?));
        }
        v = Val::Obj(kv);
    }
    if n.flags & 0x4000 != 0 {
        r.align();
    }
    Ok(v)
}

/// A decoded object as JSON (for the differential tests): ints and floats as numbers, bytes as `{"hex": ..}`, a dict as an object (the last of equal names wins)
pub fn val_json(v: &Val<'_>) -> serde_json::Value {
    use serde_json::{Value, json};
    match v {
        Val::Int(i) => json!(i),
        Val::Float(f) => json!(f),
        Val::Bool(b) => Value::Bool(*b),
        Val::Str(s) => Value::String(s.to_string()),
        Val::Bytes(b) => json!({ "hex": hex(b) }),
        Val::List(l) => Value::Array(l.iter().map(val_json).collect()),
        Val::Obj(kv) => {
            let mut m = serde_json::Map::new();
            for (k, x) in kv {
                m.insert((*k).to_string(), val_json(x));
            }
            Value::Object(m)
        }
    }
}

/// One object of the type tree `root` decoded from `data` (the Python's `_read(Reader(data, le), root)`)
pub fn object_json(root: &Node, data: &[u8], le: bool) -> Res<serde_json::Value> {
    let mut rd = Rd { d: data, p: 0, le };
    let v = read_object(&mut rd, root)?;
    Ok(val_json(&v))
}

/// A type tree as `[type, name, flags, [kids]]` (for the differential tests)
pub fn node_json(n: &Node) -> serde_json::Value {
    serde_json::json!([
        n.ty,
        n.name,
        n.flags,
        n.kids.iter().map(node_json).collect::<Vec<_>>()
    ])
}

// ---- a type tree written out as a layout (KNOWN_TREES) ------------------------------------------------------------------------

fn split_top(s: &str, sep: char) -> Vec<String> {
    let (mut out, mut depth, mut cur) = (Vec::new(), 0i32, String::new());
    for ch in s.chars() {
        if ch == '<' || ch == '(' {
            depth += 1;
        } else if ch == '>' || ch == ')' {
            depth -= 1;
        }
        if ch == sep && depth == 0 {
            out.push(std::mem::take(&mut cur));
        } else {
            cur.push(ch);
        }
    }
    out.push(cur);
    out
}

/// `tree_from_layout(root, layout)`: a KNOWN_TREES entry to the type tree's root node
pub fn tree_from_layout(root: &str, layout: &str) -> Result<Node, String> {
    let mut structs: HashMap<&str, Vec<(String, String)>> = HashMap::new();
    for line in layout.trim().split('\n') {
        let (name, body) = line
            .split_once(": ")
            .ok_or_else(|| format!("layout line without ': ': {line:?}"))?;
        let fields = body
            .split("; ")
            .map(|f| {
                f.rsplit_once(' ')
                    .map(|(t, n)| (t.to_string(), n.to_string()))
                    .ok_or_else(|| format!("layout field without a name: {f:?}"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        structs.insert(name, fields);
    }
    fn node(
        t: &str,
        name: &str,
        structs: &HashMap<&str, Vec<(String, String)>>,
        depth: usize,
    ) -> Result<Node, String> {
        if depth > 64 {
            return Err("type tree layout nests too deep".into());
        }
        let mut t = t;
        let mut flags = 0;
        if let Some(r) = t.strip_suffix('!') {
            t = r;
            flags = 0x4000;
        }
        let mut arr_align = 0x4000;
        if let Some(r) = t.strip_suffix('~') {
            t = r;
            arr_align = 0;
        }
        let array = |el: Node| {
            let mut a = Node::new("Array", "Array", arr_align);
            a.kids = vec![Node::new("int", "size", 0), el];
            a
        };
        if t == "string" {
            let mut n = Node::new("string", name, flags);
            n.kids = vec![array(Node::new("char", "data", 0))];
            Ok(n)
        } else if t.starts_with("vector<")
            || t.starts_with("map<")
            || t.starts_with("staticvector<")
        {
            let inner_all = &t[..t.len() - 1];
            let (kind, inner) = inner_all.split_once('<').unwrap();
            let (el_name, el_type) = inner
                .split_once(':')
                .ok_or_else(|| format!("bad container {t}"))?;
            let mut n = Node::new(kind, name, flags);
            n.kids = vec![array(node(el_type, el_name, structs, depth + 1)?)];
            Ok(n)
        } else if t.starts_with("pair(") {
            let parts = split_top(&t[5..t.len() - 1], ',');
            if parts.len() != 2 {
                return Err(format!("bad pair {t}"));
            }
            let mut n = Node::new("pair", name, flags);
            n.kids = vec![
                node(&parts[0], "first", structs, depth + 1)?,
                node(&parts[1], "second", structs, depth + 1)?,
            ];
            Ok(n)
        } else if let Some(fields) = structs.get(t) {
            let mut n = Node::new(t, name, flags);
            for (ft, fname) in fields {
                n.kids.push(node(ft, fname, structs, depth + 1)?);
            }
            Ok(n)
        } else if prim(t).is_some() {
            Ok(Node::new(t, name, flags))
        } else {
            Err(format!("type tree layout: unknown type {t}"))
        }
    }
    node(root, "Base", &structs, 0)
}

/// A known tree: ((version, hash), tree).
pub type KnownTree = ((i32, [u8; 16]), Arc<Node>);

/// `unityfs.KNOWN_TREES` as trees (`known_tree` for each entry)
pub fn known_trees() -> Vec<KnownTree> {
    KNOWN_TREES
        .iter()
        .map(|(cid, h, root, layout)| {
            (
                (*cid, *h),
                Arc::new(
                    tree_from_layout(root, layout)
                        .expect("a KNOWN_TREES layout parses (checked by the unit tests)"),
                ),
            )
        })
        .collect()
}

// ---- serialized files ----------------------------------------------------------------------------------------------------------

/// `is_serialized(d)`: does it look like a serialized file (big-endian header: format 17..22; from 22 the sizes follow +0x14). The first 48 bytes and the length decide.
pub fn is_serialized(head: &[u8], total: u64) -> bool {
    if total < 48 || head.len() < 48 {
        return false;
    }
    let ver = u32::from_be_bytes(head[8..12].try_into().unwrap());
    if !(17..=22).contains(&ver) || head[16] > 1 {
        return false;
    }
    let (msz, fsz, doff): (u64, i64, i64) = if ver >= 22 {
        (
            u32::from_be_bytes(head[20..24].try_into().unwrap()) as u64,
            i64::from_be_bytes(head[24..32].try_into().unwrap()),
            i64::from_be_bytes(head[32..40].try_into().unwrap()),
        )
    } else {
        (
            u32::from_be_bytes(head[0..4].try_into().unwrap()) as u64,
            u32::from_be_bytes(head[4..8].try_into().unwrap()) as i64,
            u32::from_be_bytes(head[12..16].try_into().unwrap()) as i64,
        )
    };
    fsz == total as i64 && 0 < msz && (msz as i64) < fsz && doff <= fsz
}

pub struct Tables {
    le: bool,
    /// the data offset and each object's start as the file has them (signed: the Python adds them as unbounded ints and slices with them)
    doff: i64,
    tlist: Vec<(i32, [u8; 16], Option<Arc<Node>>)>,
    objs: Vec<(i64, i64, u32, i32)>,
}

impl Tables {
    /// `d[doff + start:doff + start + size]` as `(offset, length)` in a file of `total` bytes, with Python's slice rules (a negative bound counts from the end,
    /// both are clamped to the data). The sum is taken in i128: it cannot wrap.
    fn object_range(&self, start: i64, size: u32, total: u64) -> (u64, usize) {
        let total = total as i128;
        let a = self.doff as i128 + start as i128;
        let b = a + size as i128;
        let fix = |i: i128| (if i < 0 { i + total } else { i }).clamp(0, total);
        let (a, b) = (fix(a), fix(b));
        (a as u64, (b - a).max(0) as usize)
    }
}

/// `_tables(d, common, want, trees)`: the type table and the object table of a serialized file. `trees` (the file's own dict, or the borrowed ones) gets this file's trees of the wanted classes.
pub fn tables(
    src: &dyn Source,
    common: Option<&HashMap<u32, String>>,
    want: Option<&[i32]>,
    trees: Option<&mut Trees>,
) -> Res<Tables> {
    let total = src.len();
    let head = src
        .read(0, 48)
        .map_err(|e| Failure::Red(format!("OSError: {e}")))?;
    let ver = u32be(&head, 8)?;
    if !(17..=22).contains(&ver) {
        red!("ValueError: serialized file format {ver} is not supported");
    }
    // the metadata is everything before the object data
    let doff = if ver >= 22 {
        i64be(&head, 32)?
    } else {
        u32be(&head, 12)? as i64
    };
    let meta_len = if doff <= 0 {
        0
    } else {
        (doff as u64).min(total)
    } as usize;
    let d = src
        .read(0, meta_len.max(48))
        .map_err(|e| Failure::Red(format!("OSError: {e}")))?;
    let mut p = 16usize;
    let le = u8at(&d, p)? == 0;
    p += 4;
    let doff_u = if ver >= 22 {
        doff
    } else {
        u32be(&d, 12)? as i64
    };
    if ver >= 22 {
        p += 28;
    }
    let mut r = Rd { d: &d, p, le };
    let (_, np) = cstr(&d, r.p)?;
    r.p = np;
    r.i32()?;
    let tt = r.u8()? != 0;
    let mut tlist = Vec::new();
    let mut trees = trees;
    let ntypes = r.i32()?;
    for _ in 0..ntypes.max(0) {
        let cid = r.i32()?;
        r.u8()?;
        r.i16()?;
        if cid == 114 {
            r.p += 16;
        }
        let hs = sl(&d, r.p, r.p + 16);
        let mut h = [0u8; 16];
        h[..hs.len()].copy_from_slice(hs);
        r.p += 16;
        let mut root: Option<Arc<Node>> = None;
        if tt {
            let nn = r.i32()?;
            let sb = r.i32()?;
            struct Raw {
                lv: u8,
                to: u32,
                no: u32,
                mf: i32,
            }
            let mut raw = Vec::new();
            for _ in 0..nn.max(0) {
                r.i16()?; // version
                let lv = r.u8()?;
                r.u8()?; // type flags
                let to = r.u32()?;
                let no = r.u32()?;
                r.i32()?; // byte size
                r.i32()?; // index
                let mf = r.i32()?;
                if ver >= 19 {
                    r.u64()?; // refTypeHash
                }
                raw.push(Raw { lv, to, no, mf });
            }
            if sb < 0 {
                red!("negative string buffer size");
            }
            let buf = sl(&d, r.p, r.p + sb as usize);
            r.p += sb as usize;
            let st = |o: u32| -> Res<String> {
                if o & 0x8000_0000 != 0 {
                    let k = o & 0x7fff_ffff;
                    match common {
                        Some(c) => Ok(c.get(&k).cloned().unwrap_or_else(|| format!("?{k}"))),
                        None => red!(
                            "AttributeError: 'NoneType' object has no attribute 'get' (no common strings)"
                        ),
                    }
                } else {
                    let o = o as usize;
                    match find(buf, &[0], o) {
                        Some(e) => Ok(String::from_utf8_lossy(&buf[o..e]).into_owned()),
                        None => red!("ValueError: subsection not found (type tree string)"),
                    }
                }
            };
            // an arena of nodes with child lists, then the tree of the last root
            struct B {
                ty: String,
                name: String,
                flags: i32,
                kids: Vec<usize>,
            }
            let mut arena: Vec<B> = Vec::with_capacity(raw.len());
            let mut stack: Vec<usize> = Vec::new();
            let mut root_i: Option<usize> = None;
            for rw in &raw {
                let i = arena.len();
                arena.push(B {
                    ty: st(rw.to)?,
                    name: st(rw.no)?,
                    flags: rw.mf,
                    kids: Vec::new(),
                });
                stack.truncate(rw.lv as usize);
                if let Some(&top) = stack.last() {
                    arena[top].kids.push(i);
                } else {
                    root_i = Some(i);
                }
                stack.push(i);
            }
            fn build(arena: &mut Vec<B>, i: usize) -> Node {
                let kids = std::mem::take(&mut arena[i].kids);
                let mut n = Node {
                    ty: std::mem::take(&mut arena[i].ty),
                    name: std::mem::take(&mut arena[i].name),
                    flags: arena[i].flags,
                    kids: Vec::new(),
                };
                for k in kids {
                    let c = build(arena, k);
                    n.kids.push(c);
                }
                n
            }
            root = root_i.map(|i| Arc::new(build(&mut arena, i)));
            if ver >= 21 {
                let n = r.i32()?;
                r.p = r.p.saturating_add(4usize.wrapping_mul(n.max(0) as usize));
            }
            if let Some(t) = trees.as_deref_mut()
                && want.is_none_or(|w| w.contains(&cid))
            {
                t.entry((cid, h)).or_insert_with(|| root.clone());
            }
        } else if let Some(t) = trees.as_deref() {
            root = t.get(&(cid, h)).cloned().flatten();
        }
        tlist.push((cid, h, root));
    }
    let mut objs = Vec::new();
    let nobj = r.i32()?;
    for _ in 0..nobj.max(0) {
        r.align();
        let pid = r.i64()?;
        let start = if ver >= 22 { r.i64()? } else { r.u32()? as i64 };
        let size = r.u32()?;
        let ti = r.i32()?;
        objs.push((pid, start, size, ti));
    }
    Ok(Tables {
        le,
        doff: doff_u,
        tlist,
        objs,
    })
}

/// `serialized_objects(d, common, want, trees)`: calls `f(class ID, path ID, object)` for each object of a wanted class, decoded by its type tree.
/// `NoTree` when a wanted object's class has no tree (in the file or borrowed).
pub fn for_each_object(
    src: &dyn Source,
    common: Option<&HashMap<u32, String>>,
    want: &[i32],
    trees: Option<&mut Trees>,
    mut f: impl FnMut(i32, i64, &Val<'_>) -> Res<()>,
) -> Res<()> {
    let t = tables(src, common, Some(want), trees)?;
    for &(pid, start, size, ti) in &t.objs {
        let (cid, h, root) = match usize::try_from(ti).ok().and_then(|i| t.tlist.get(i)) {
            Some(x) => x,
            None => red!("IndexError: type index {ti} out of range"),
        };
        if !want.is_empty() && !want.contains(cid) {
            continue;
        }
        let (off, n) = t.object_range(start, size, src.len());
        let data = src
            .read(off, n)
            .map_err(|e| Failure::Red(format!("OSError: {e}")))?;
        if let Some(root) = root {
            let mut rd = Rd {
                d: &data,
                p: 0,
                le: t.le,
            };
            let v = read_object(&mut rd, root)?;
            f(*cid, pid, &v)?;
            continue;
        }

        // Some Unity 2018/2019 serialized files strip the type tree but use a
        // layout already present in another Unity version. INSIDE is one
        // example: refusing the whole file leaves its runtime programs out of
        // shader_map.tsv, so every draw later has no_pipeline. Try the shipped
        // Shader layouts only after the exact (class, hash) lookup failed.
        // Files with an exact tree keep the byte-for-byte old path above.
        let candidate = if *cid == CLASS_SHADER {
            known_trees()
                .into_iter()
                .filter(|((class, _), _)| *class == CLASS_SHADER)
                .filter_map(|((_, _), candidate)| {
                    let mut rd = Rd {
                        d: &data,
                        p: 0,
                        le: t.le,
                    };
                    let value = read_object(&mut rd, &candidate).ok()?;
                    let parsed = value.get("m_ParsedForm")?;
                    (parsed.get("m_SubShaders").is_some() || value.get("m_Name").is_some())
                        .then_some(value)
                })
                .next()
        } else {
            None
        };
        let Some(value) = candidate else {
            return Err(Failure::NoTree(format!(
                "class {cid} (type hash {}) has no type tree available",
                hex(h)
            )));
        };
        f(*cid, pid, &value)?;
    }
    Ok(())
}

/// `text_assets(d, common)`: `f(path ID, name, script bytes)` of each TextAsset (class 49). The object starts with m_Name then m_Script (an aligned string each) in every Unity
/// version these tools read, so it is read without a type tree, also from files that carry none
pub fn for_each_text_asset(
    src: &dyn Source,
    common: Option<&HashMap<u32, String>>,
    mut f: impl FnMut(i64, &str, &[u8]) -> Res<()>,
) -> Res<()> {
    let t = tables(src, common, Some(&[CLASS_TEXT_ASSET]), None)?;
    for &(pid, start, size, ti) in &t.objs {
        let cid = match usize::try_from(ti).ok().and_then(|i| t.tlist.get(i)) {
            Some(x) => x.0,
            None => red!("IndexError: type index {ti} out of range"),
        };
        if cid != CLASS_TEXT_ASSET {
            continue;
        }
        let (off, n) = t.object_range(start, size, src.len());
        let data = src
            .read(off, n)
            .map_err(|e| Failure::Red(format!("OSError: {e}")))?;
        let mut r = Rd {
            d: &data,
            p: 0,
            le: t.le,
        };
        if data.len() < 8 {
            continue;
        }
        let ln = r.i32()?;
        // the -8 is the two 4-byte length fields
        if ln < 0 || ln as usize > data.len() - 8 {
            continue;
        }
        let name = String::from_utf8_lossy(sl(&data, r.p, r.p + ln as usize)).into_owned();
        r.p += ln as usize;
        r.align();
        if r.p + 4 > data.len() {
            continue;
        }
        let ln = r.i32()?;
        if ln < 0 || ln as usize > data.len() - r.p {
            continue;
        }
        f(pid, &name, sl(&data, r.p, r.p + ln as usize))?;
    }
    Ok(())
}

// ---- a Shader's programs -------------------------------------------------------------------------------------------------------

/// The programs of one Shader object: the decompressed segments (LZ4) and where each subprogram sits in them. A subprogram's bytes are borrowed from the segment, never copied
/// (the Python's `shader_programs` yields `bytes` copies; the readers only look at them).
pub struct Programs {
    segs: Vec<Vec<u8>>,
    /// `(platform, subprogram index, segment, start, end)` in the order `shader_programs` yields them
    items: Vec<(i64, i64, usize, usize, usize)>,
}

impl Programs {
    /// `(platform, subprogram index, subprogram bytes)`
    pub fn iter(&self) -> impl Iterator<Item = (i64, i64, &[u8])> + '_ {
        self.items
            .iter()
            .map(|&(plat, k, seg, a, b)| (plat, k, &self.segs[seg][a..b]))
    }
}

/// `d[a:a+n]` as a `(start, end)` pair (Python slicing: clamped)
fn range_of(len: usize, a: usize, n: usize) -> (usize, usize) {
    let b = a.saturating_add(n).min(len);
    (a.min(b), b)
}

/// `shader_programs(sh, only)`: `[(platform, subprogram index, subprogram bytes)]`; `only` = the subprogram indices to yield (None: all)
pub fn shader_programs(
    sh: &Val<'_>,
    only: Option<&std::collections::HashSet<i64>>,
) -> Res<Programs> {
    let mut out = Programs {
        segs: Vec::new(),
        items: Vec::new(),
    };
    let key = |name: &str| -> Res<&Val<'_>> {
        match sh.get(name) {
            Some(v) => Ok(v),
            None => red!("KeyError: {name}"),
        }
    };
    let blob = key("compressedBlob")?.bytes();
    let (plats, offsets, clens, dlens) = (
        key("platforms")?,
        key("offsets")?,
        key("compressedLengths")?,
        key("decompressedLengths")?,
    );
    for (i, plat) in plats.list().iter().enumerate() {
        let plat = plat.int().unwrap_or(0);
        let (offs, clen, dlen) = (
            offsets
                .list()
                .get(i)
                .ok_or_else(|| Failure::Red("IndexError: offsets".into()))?,
            clens
                .list()
                .get(i)
                .ok_or_else(|| Failure::Red("IndexError: compressedLengths".into()))?,
            dlens
                .list()
                .get(i)
                .ok_or_else(|| Failure::Red("IndexError: decompressedLengths".into()))?,
        );
        if !matches!(offs, Val::List(_)) {
            // before 2019.3: one segment per platform, 8-byte entries {offset, length}
            let (o, c, u) = (
                offs.int().unwrap_or(0).max(0) as usize,
                clen.int().unwrap_or(0).max(0) as usize,
                dlen.int().unwrap_or(0).max(0) as usize,
            );
            let s0 = lz4(at(blob, o, c), u)?;
            let n = u32le(&s0, 0)?;
            let si = out.segs.len();
            for k in 0..n as usize {
                let (off, ln) = (
                    u32le(&s0, 4 + 8 * k)? as usize,
                    u32le(&s0, 8 + 8 * k)? as usize,
                );
                if only.is_none_or(|s| s.contains(&(k as i64))) {
                    let (a, b) = range_of(s0.len(), off, ln);
                    out.items.push((plat, k as i64, si, a, b));
                }
            }
            out.segs.push(s0);
            continue;
        }
        let first = out.segs.len();
        for ((o, c), u) in offs.list().iter().zip(clen.list()).zip(dlen.list()) {
            let seg = lz4(
                at(
                    blob,
                    o.int().unwrap_or(0).max(0) as usize,
                    c.int().unwrap_or(0).max(0) as usize,
                ),
                u.int().unwrap_or(0).max(0) as usize,
            )?;
            out.segs.push(seg);
        }
        if out.segs.len() == first {
            continue;
        }
        let n = u32le(&out.segs[first], 0)?;
        for k in 0..n as usize {
            let (off, ln, seg) = (
                u32le(&out.segs[first], 4 + 12 * k)? as usize,
                u32le(&out.segs[first], 8 + 12 * k)? as usize,
                u32le(&out.segs[first], 12 + 12 * k)? as usize,
            );
            if only.is_none_or(|s| s.contains(&(k as i64))) {
                let sgm = match out
                    .segs
                    .get(first + seg)
                    .filter(|_| seg < out.segs.len() - first)
                {
                    Some(s) => s,
                    None => red!("IndexError: segment {seg}"),
                };
                let (a, b) = range_of(sgm.len(), off, ln);
                out.items.push((plat, k as i64, first + seg, a, b));
            }
        }
    }
    Ok(out)
}

/// `subprogram(b)[4]`: the program code of a subprogram (header: version, program type, stats, keywords, then the code bytes)
pub fn subprogram_code(b: &[u8]) -> Res<&[u8]> {
    let mut r = Rd {
        d: b,
        p: 0,
        le: true,
    };
    let ver = r.i32()?;
    r.i32()?;
    r.p += 12;
    if ver >= 201608170 {
        r.p += 4;
    }
    fn strs(r: &mut Rd<'_>) -> Res<()> {
        let n = r.i32()?;
        for _ in 0..n.max(0) {
            let ln = r.i32()?;
            if ln < 0 {
                red!("negative keyword length");
            }
            r.p += ln as usize;
            r.align();
        }
        Ok(())
    }
    strs(&mut r)?; // keywords
    if (201806140..202012090).contains(&ver) {
        strs(&mut r)?; // local keywords
    }
    let ln = r.i32()?;
    if ln < 0 {
        red!("negative program code length");
    }
    Ok(sl(b, r.p, r.p + ln as usize))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_known_layout_parses_into_a_tree() {
        let t = known_trees();
        assert_eq!(t.len(), KNOWN_TREES.len());
        assert_eq!(t.len(), 16);
        for ((cid, _), n) in &t {
            assert!(
                matches!(n.ty.as_str(), "Shader" | "ComputeShader"),
                "class {cid}: {}",
                n.ty
            );
            assert_eq!(n.name, "Base");
            assert!(!n.kids.is_empty());
        }
    }

    #[test]
    fn a_layout_node_carries_the_alignment_flags() {
        // `~` = the container's array is not aligned, `!` = the field itself is
        let n = tree_from_layout("A", "A: vector<data:int>~! v; string s; int x[0]").unwrap();
        assert_eq!(n.kids[0].flags, 0x4000);
        assert_eq!(n.kids[0].kids[0].flags, 0);
        assert_eq!(n.kids[1].kids[0].flags, 0x4000); // a string's char array is aligned
        assert_eq!(n.kids[2].ty, "int");
        assert!(tree_from_layout("A", "A: nonsense x").is_err());
    }

    #[test]
    fn a_decoded_object_follows_the_alignment() {
        // string "ab" (4 + 2 bytes, aligned to 4), then an int
        let n = tree_from_layout("A", "A: string s; int x").unwrap();
        let mut d = vec![2, 0, 0, 0, b'a', b'b', 0, 0, 7, 0, 0, 0];
        let v = object_json(&n, &d, true).unwrap();
        assert_eq!(v, serde_json::json!({"s": "ab", "x": 7}));
        d.truncate(9);
        assert!(object_json(&n, &d, true).is_err());
    }

    #[test]
    fn an_array_of_elements_that_read_nothing_is_bounded() {
        // A { vector<data:E> v } with E a struct without fields: each element reads no byte
        let mut arr = Node::new("Array", "Array", 0);
        arr.kids = vec![Node::new("int", "size", 0), Node::new("E", "data", 0)];
        let mut vec_n = Node::new("vector", "v", 0);
        vec_n.kids = vec![arr];
        let mut root = Node::new("A", "Base", 0);
        root.kids = vec![vec_n];
        // a small count (no more than the bytes left) decodes as the Python does
        let d = [3u8, 0, 0, 0, 9, 9, 9];
        assert_eq!(
            object_json(&root, &d, true).unwrap(),
            serde_json::json!({"v": [{}, {}, {}]})
        );
        // 2^31 - 1 elements: an error, not 2^31 pushes
        let d = [0xff, 0xff, 0xff, 0x7f, 0, 0];
        assert!(matches!(
            object_json(&root, &d, true),
            Err(Failure::Red(m)) if m.contains("read no bytes")
        ));
    }

    /// `A { vector<S> v }` with `S { vector<E> w }` and `E` a structure without fields: each outer element reads only its inner count (4 bytes), the inner arrays read nothing
    fn nested_empty() -> Node {
        let vec_of = |name: &str, el: Node| {
            let mut arr = Node::new("Array", "Array", 0);
            arr.kids = vec![Node::new("int", "size", 0), el];
            let mut v = Node::new("vector", name, 0);
            v.kids = vec![arr];
            v
        };
        let mut s = Node::new("S", "data", 0);
        s.kids = vec![vec_of("w", Node::new("E", "data", 0))];
        let mut root = Node::new("A", "Base", 0);
        root.kids = vec![vec_of("v", s)];
        root
    }

    /// the object of `nested_empty` with `outer` elements, inner element `i` declaring `inner(i, bytes left after its count)` empty values
    fn nested_data(outer: usize, inner: impl Fn(usize, usize) -> u32) -> Vec<u8> {
        let len = 4 + 4 * outer;
        let mut d = (outer as u32).to_le_bytes().to_vec();
        for i in 0..outer {
            let left = len - 4 * (i + 2);
            d.extend_from_slice(&inner(i, left).to_le_bytes());
        }
        d
    }

    fn values(v: &serde_json::Value) -> usize {
        1 + match v {
            serde_json::Value::Array(a) => a.iter().map(values).sum(),
            serde_json::Value::Object(o) => o.values().map(values).sum(),
            _ => 0,
        }
    }

    #[test]
    fn nested_arrays_of_elements_that_read_nothing_cannot_amplify_quadratically() {
        // review r4 4.4: each inner array passed the per-array guard (no more elements than bytes left), so 3,804 bytes decoded to ~1.8 million values and the call was `Ok`
        let root = nested_empty();
        let d = nested_data(950, |_, left| left as u32);
        assert_eq!(d.len(), 3804);
        let e = object_json(&root, &d, true).unwrap_err();
        assert!(
            matches!(&e, Failure::Red(m) if m.starts_with("MemoryError:") && m.contains("3804 bytes") && m.contains(&node_budget(3804).to_string())),
            "{e:?}"
        );
        // the same shape with a count past the bytes left is still the per-array refusal (its message is unchanged)
        let d = nested_data(4, |i, left| if i == 2 { left as u32 + 1 } else { 0 });
        assert!(
            matches!(object_json(&root, &d, true), Err(Failure::Red(m)) if m.contains("read no bytes"))
        );
        // the same shape at a size the Python decodes in a moment is decoded as before: 250 outer elements of 3 empty values each (the last one has no byte left, so 0)
        let d = nested_data(250, |_, left| 3.min(left) as u32);
        let v = object_json(&root, &d, true).unwrap();
        assert_eq!(v["v"].as_array().unwrap().len(), 250);
        assert_eq!(v["v"][248], serde_json::json!({"w": [{}, {}, {}]}));
        assert_eq!(v["v"][249], serde_json::json!({"w": []}));
        assert_eq!(values(&v), 2 + 249 * (2 + 3) + 2);
        // the largest decode of this shape the budget lets through: the inner arrays fill their room until the values reach 8 x len + 4096 (quadratic growth meets a linear bound)
        let outer = 600; // 2,404 bytes, a budget of 23,328 values
        let d = nested_data(outer, |i, left| if i < 6 { left as u32 } else { 0 });
        let v = object_json(&root, &d, true).unwrap();
        assert!(values(&v) <= node_budget(d.len()), "{}", values(&v));
        let d = nested_data(outer, |i, left| if i < 12 { left as u32 } else { 0 });
        assert!(
            object_json(&root, &d, true).is_err(),
            "12 full inner arrays pass the budget"
        );
    }

    #[test]
    fn the_budget_lets_the_flat_case_through_and_counts_primitive_arrays() {
        // 1,000 empty values in 1,004 bytes: within the per-array guard and the budget (8 x 1,004 + 4,096), decoded as before
        let mut arr = Node::new("Array", "Array", 0);
        arr.kids = vec![Node::new("int", "size", 0), Node::new("E", "data", 0)];
        let mut vec_n = Node::new("vector", "v", 0);
        vec_n.kids = vec![arr];
        let mut root = Node::new("A", "Base", 0);
        root.kids = vec![vec_n];
        let mut d = 1000u32.to_le_bytes().to_vec();
        d.extend_from_slice(&[0u8; 1000]);
        let v = object_json(&root, &d, true).unwrap();
        assert_eq!(values(&v), 1002);
        // an array of primitives spends one value per element: 2^20 bytes of u32s decode (2^18 values), they fit
        let mut arr = Node::new("Array", "Array", 0);
        arr.kids = vec![Node::new("int", "size", 0), Node::new("UInt32", "data", 0)];
        let mut vec_n = Node::new("vector", "v", 0);
        vec_n.kids = vec![arr];
        let mut root = Node::new("A", "Base", 0);
        root.kids = vec![vec_n];
        let n = 1usize << 18;
        let mut d = (n as u32).to_le_bytes().to_vec();
        d.extend(std::iter::repeat_n(7u8, 4 * n));
        assert_eq!(
            object_json(&root, &d, true).unwrap()["v"]
                .as_array()
                .unwrap()
                .len(),
            n
        );
        let mut left = 3usize;
        assert!(spend(&mut left, 3, 0).is_ok() && left == 0 && spend(&mut left, 1, 0).is_err());
    }

    /// (values, bytes) of one read of `n` with every array empty, and the most values per byte an element of one of its arrays decodes to (honest data: every count is the file's own)
    fn honest_cost(n: &Node) -> (f64, f64, f64) {
        if let (Some(p), true) = (prim(&n.ty), n.kids.is_empty()) {
            return (1.0, p.size() as f64, 0.0);
        }
        if n.ty == "string" || n.ty == "TypelessData" {
            return (1.0, 4.0, 0.0);
        }
        if let Some(arr) = n.kids.first().filter(|k| k.ty == "Array") {
            let el = &arr.kids[1];
            let per = if matches!(el.ty.as_str(), "UInt8" | "char" | "SInt8") && el.kids.is_empty()
            {
                0.0
            } else if let (Some(p), true) = (prim(&el.ty), el.kids.is_empty()) {
                1.0 / p.size() as f64
            } else {
                let (v, b, w) = honest_cost(el);
                (if b > 0.0 { v / b } else { f64::INFINITY }).max(w)
            };
            return (1.0, 4.0, per);
        }
        let (mut v, mut b, mut w) = (1.0, 0.0, 0.0f64);
        for k in &n.kids {
            let (kv, kb, kw) = honest_cost(k);
            v += kv;
            b += kb;
            w = w.max(kw);
        }
        (v, b, w)
    }

    #[test]
    fn honest_objects_stay_far_under_the_budget() {
        // every element of every array of the shipped Shader / ComputeShader layouts reads bytes, and decodes to at most 1.5 values per byte (asserted: 2, a quarter of the budget's 8)
        let mut worst = 0.0f64;
        for ((cid, _), t) in known_trees() {
            let (v, b, w) = honest_cost(&t);
            let r = (v / b.max(1.0)).max(w);
            assert!(r.is_finite(), "class {cid}: an element that reads nothing");
            worst = worst.max(r);
        }
        assert!(worst <= 2.0, "{worst}");
        assert!(worst * 4.0 <= NODES_PER_BYTE as f64, "{worst}");
    }

    #[test]
    fn an_object_range_follows_python_slicing_and_cannot_wrap() {
        let t = |doff: i64| Tables {
            le: true,
            doff,
            tlist: Vec::new(),
            objs: Vec::new(),
        };
        // d[100 + 10:100 + 10 + 20] of 1000 bytes
        assert_eq!(t(100).object_range(10, 20, 1000), (110, 20));
        // clamped to the data
        assert_eq!(t(100).object_range(890, 20, 1000), (990, 10));
        assert_eq!(t(100).object_range(i64::MAX, 20, 1000), (1000, 0));
        // a negative start (a v22 file's i64): d[90:110], not a wrapped u64 sum
        assert_eq!(t(100).object_range(-10, 20, 1000), (90, 20));
        // a negative sum counts from the end, as a Python slice does: d[-5:15] of 1000 bytes is empty, d[-30:-10] is d[970:990]
        assert_eq!(t(0).object_range(-5, 20, 1000), (995, 0));
        assert_eq!(t(0).object_range(-30, 20, 1000), (970, 20));
        assert_eq!(t(i64::MIN).object_range(i64::MIN, u32::MAX, 1000), (0, 0));
    }

    #[test]
    fn common_strings_stop_at_the_first_non_text() {
        let mut img = vec![9u8; 10];
        img.extend_from_slice(b"AABB\0AnimationClip\0AnimationCurve\0Zed\0\0tail");
        let t = common_strings(&img).unwrap();
        assert_eq!(t[&0], "AABB");
        assert_eq!(t[&5], "AnimationClip");
        assert_eq!(t.len(), 4);
        assert!(common_strings(b"nothing here").is_none());
    }
}
