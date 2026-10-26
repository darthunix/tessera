//! Machine code of two revisions, compared function by function.
//!
//! A change meant to keep the hot loops as they are (a refactoring, a
//! panic path removed elsewhere) is accepted when the bodies of Tessera's
//! functions in the benchmark programs stay the same. Both revisions are
//! built with one codegen unit, so that a change in one function does not
//! move the others between units; objdump lists each program, and every
//! function whose name mentions Tessera is kept with its addresses
//! normalized: the address of an instruction, a branch's absolute target
//! and a symbol's hash differ between two builds of the same code, so a
//! target is kept as the symbol and offset objdump names, and other
//! absolute addresses become `ADDR`. Data moves too when constants are
//! added or removed elsewhere: on AArch64 the page an `adrp` loads becomes
//! `ADDR` and the offset within it, an immediate taken from that register
//! until it is written again, becomes `LO12`; on x86-64 a displacement
//! from the instruction pointer and objdump's comment on it become `ADDR`.
//! Registers, other immediates and the order of the instructions are
//! kept: with one codegen unit the same source gives the same code. A
//! changed function is then told apart when only its immediates differ,
//! or only the numbers of its registers ([`registers_only`]).

use anyhow::{Context, Result, ensure};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    process::Command,
};

/// A program's functions: per name, the bodies under that name (closures
/// and generic instances may share one after normalization).
pub type Functions = BTreeMap<String, Vec<String>>;

/// objdump's listing of a program or a library.
pub fn listing(executable: &Path) -> Result<String> {
    let output = Command::new("objdump")
        .args(["-d", "--no-show-raw-insn", "--demangle"])
        .arg(executable)
        .output()
        .context("cannot start objdump")?;
    ensure!(
        output.status.success(),
        "objdump {}: {}",
        executable.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Whether a function is Tessera's: its path or an instance's type
/// mentions a Tessera crate, or it is an entry point of the C API.
fn tessera(name: &str) -> bool {
    name.contains("tessera") || name.trim_start_matches('_').starts_with("tess_")
}

/// Parse objdump's listing into Tessera's functions.
pub fn parse(listing: &str) -> Functions {
    parse_kept(listing, tessera)
}

/// Parse objdump's listing of one of Tessera's own libraries: every
/// function in it is the extension's (a C module and what it links in).
pub fn parse_all(listing: &str) -> Functions {
    parse_kept(listing, |_| true)
}

fn parse_kept(listing: &str, kept: impl Fn(&str) -> bool) -> Functions {
    let mut functions = Functions::new();
    let mut current: Option<(String, Vec<String>)> = None;
    let mut pages = Pages::default();
    let mut finish = |current: &mut Option<(String, Vec<String>)>| {
        if let Some((name, body)) = current.take() {
            functions.entry(name).or_default().push(body.join("\n"));
        }
    };
    for line in listing.lines() {
        if let Some(name) = header(line) {
            finish(&mut current);
            pages = Pages::default();
            if kept(&name) {
                current = Some((name, Vec::new()));
            }
        } else if let Some((name, body)) = current.as_mut()
            && let Some(instruction) = instruction(line)
        {
            body.push(foreign_offsets(&pages.normalize(&instruction), name));
        }
    }
    finish(&mut current);
    for bodies in functions.values_mut() {
        bodies.sort();
    }
    functions
}

/// The name of the function a line starts: `<address> <name>:`.
fn header(line: &str) -> Option<String> {
    let (address, rest) = line.split_once(' ')?;
    if address.is_empty() || !address.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let name = rest.strip_prefix('<')?.strip_suffix(">:")?;
    Some(fragments(&strip_hashes(name)))
}

/// The pieces the compiler cuts out of functions are numbered in the order
/// it makes them, which moves when other code does: `OUTLINED_FUNCTION_12`
/// (a sequence several functions share) and `f.cold.3` (a cold path of
/// `f`) become `OUTLINED_FUNCTION_N` and `f.cold.N`, the bodies under one
/// name compared as a set.
fn fragments(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut rest = name;
    loop {
        let next = ["OUTLINED_FUNCTION_", ".cold."]
            .iter()
            .filter_map(|marker| rest.find(marker).map(|at| (at, marker.len())))
            .min();
        let Some((at, length)) = next else {
            out.push_str(rest);
            return out;
        };
        let tail = &rest[at + length..];
        let digits = tail.bytes().take_while(u8::is_ascii_digit).count();
        out.push_str(&rest[..at + length]);
        if digits > 0 {
            out.push('N');
        }
        rest = &tail[digits..];
    }
}

/// Whether a name is a piece the compiler cut out of a function.
pub fn fragment(name: &str) -> bool {
    name.contains("OUTLINED_FUNCTION_") || name.contains(".cold.")
}

/// An instruction line, `<address>: <instruction>`, without its address
/// and with its absolute addresses normalized.
fn instruction(line: &str) -> Option<String> {
    let (address, rest) = line.trim_start().split_once(':')?;
    if address.is_empty() || !address.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let text = rest.trim();
    (!text.is_empty()).then(|| normalize(text))
}

/// The AArch64 registers holding a page an `adrp` loaded, within one
/// function: an immediate offset from one of them is the low bits of a
/// data address.
#[derive(Default)]
struct Pages {
    registers: BTreeSet<u32>,
}

impl Pages {
    fn normalize(&mut self, text: &str) -> String {
        if let Some(at) = text.find("(%rip)")
            && let Some(comment) = text[at..].find(" #")
        {
            return text[..at + comment].trim_end().to_owned();
        }
        let (mnemonic, operands) = text
            .split_once(|c: char| c.is_ascii_whitespace())
            .map_or((text, ""), |(m, o)| (m, o.trim_start()));
        let parts: Vec<&str> = operands.split(", ").collect();
        let destination = parts.first().and_then(|first| register(first));
        if mnemonic == "adrp" || mnemonic == "adr" {
            if let Some(register) = destination {
                if mnemonic == "adrp" {
                    self.registers.insert(register);
                } else {
                    self.registers.remove(&register);
                }
            }
            return format!("{mnemonic}\t{}, ADDR", parts[0]);
        }
        let mut out = Vec::with_capacity(parts.len());
        let mut base_page = false;
        for (index, part) in parts.iter().enumerate() {
            let page = register(part).is_some_and(|r| self.registers.contains(&r));
            if part.starts_with('[') && page {
                base_page = true;
                // Data at the start of its page: objdump prints no offset.
                if let Some(base) = part.strip_suffix(']') {
                    out.push(format!("{base}, LO12]"));
                    continue;
                }
            }
            let literal = index == 1
                && mnemonic.starts_with("ldr")
                && !part.starts_with('[')
                && !part.starts_with('#')
                && register(part).is_none();
            if literal {
                out.push("ADDR".to_owned());
            } else if part.starts_with('#')
                && (base_page
                    || (mnemonic == "add"
                        && index == 2
                        && parts
                            .get(1)
                            .and_then(|p| register(p))
                            .is_some_and(|r| self.registers.contains(&r))))
            {
                let closing = part.find(']').map_or("", |at| &part[at..]);
                out.push(format!("LO12{closing}"));
            } else {
                out.push((*part).to_owned());
            }
        }
        let stores =
            mnemonic.starts_with("st") || mnemonic.starts_with("cb") || mnemonic.starts_with("tb");
        if !stores && let Some(register) = destination {
            self.registers.remove(&register);
        }
        if operands.is_empty() {
            mnemonic.to_owned()
        } else {
            format!("{mnemonic}\t{}", out.join(", "))
        }
    }
}

/// An offset into another symbol, `<dyld_stub_binder+0x4f3e0>` for a call
/// through a stub or a data symbol's interior, moves when other code does:
/// it becomes `<symbol+OFF>`. Offsets into the function itself, its branch
/// targets, stay.
fn foreign_offsets(text: &str, function: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('<') {
        out.push_str(&rest[..open]);
        let Some(close) = matching(rest.as_bytes(), open) else {
            out.push_str(&rest[open..]);
            return out;
        };
        let symbol = &rest[open + 1..close - 1];
        match symbol.rsplit_once("+0x") {
            Some((name, offset))
                if name != function && offset.bytes().all(|b| b.is_ascii_hexdigit()) =>
            {
                out.push('<');
                out.push_str(name);
                out.push_str("+OFF>");
            }
            _ => out.push_str(&rest[open..close]),
        }
        rest = &rest[close..];
    }
    out.push_str(rest);
    out
}

/// The end, past its `>`, of the angle brackets opening at `open`.
fn matching(bytes: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (offset, &byte) in bytes[open..].iter().enumerate() {
        match byte {
            b'<' => depth += 1,
            b'>' => {
                depth -= 1;
                if depth == 0 {
                    return Some(open + offset + 1);
                }
            }
            _ => {}
        }
    }
    None
}

/// The number of an AArch64 general register an operand names, `x10`,
/// `w10` or `[x10`, or `None`.
fn register(operand: &str) -> Option<u32> {
    let name = operand.trim_start_matches('[');
    let digits = name.strip_prefix('x').or_else(|| name.strip_prefix('w'))?;
    let digits = digits.trim_end_matches([']', '!']);
    digits.parse().ok().filter(|&n: &u32| n < 31)
}

/// Drop the hash of a legacy-mangled Rust symbol, `::h` and 16 hex digits.
fn strip_hashes(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut rest = name;
    while let Some(at) = rest.find("::h") {
        let hash = &rest[at + 3..];
        let digits = hash.bytes().take_while(u8::is_ascii_hexdigit).count();
        if digits == 16 {
            out.push_str(&rest[..at]);
            rest = &hash[16..];
        } else {
            out.push_str(&rest[..at + 3]);
            rest = hash;
        }
    }
    out.push_str(rest);
    out
}

/// Normalize the operands of an instruction: an address followed by the
/// symbol objdump names, `0x1000 <name+0x20>`, becomes `<name+0x20>`; a
/// displacement from the instruction pointer, `0x2f0e(%rip)`, and any other
/// bare hex number becomes `ADDR`. Immediates (`#0x10`, `$0x10`) and
/// displacements from other registers (`0x10(%rsp)`) stay.
fn normalize(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    while at < bytes.len() {
        let previous = at.checked_sub(1).map(|i| bytes[i]);
        let starts_word = previous
            .is_none_or(|b| !b.is_ascii_alphanumeric() && b != b'#' && b != b'$' && b != b'_');
        if starts_word && let Some(end) = address_end(bytes, at) {
            if let Some(symbol_end) = symbol_end(bytes, end) {
                out.push('<');
                out.push_str(&fragments(&strip_hashes(&text[end + 2..symbol_end - 1])));
                out.push('>');
                at = symbol_end;
            } else if bytes[end..].starts_with(b"(") && !bytes[end..].starts_with(b"(%rip)") {
                out.push_str(&text[at..end]);
                at = end;
            } else {
                out.push_str("ADDR");
                at = end;
            }
            continue;
        }
        let char_len = text[at..].chars().next().map_or(1, char::len_utf8);
        out.push_str(&text[at..at + char_len]);
        at += char_len;
    }
    out
}

/// The end of an address at `at`: `0x` and hex digits, or six or more hex
/// digits (GNU objdump writes branch targets without `0x`).
fn address_end(bytes: &[u8], at: usize) -> Option<usize> {
    let hex = |from: usize| {
        bytes[from..]
            .iter()
            .take_while(|b| b.is_ascii_hexdigit())
            .count()
    };
    let end = if bytes[at..].starts_with(b"0x") {
        let digits = hex(at + 2);
        (digits > 0).then_some(at + 2 + digits)?
    } else {
        let digits = hex(at);
        // A bare number is an address only when long and followed by a
        // symbol, as GNU objdump writes a call's target.
        (digits >= 6 && bytes[at + digits..].starts_with(b" <")).then_some(at + digits)?
    };
    let next = bytes.get(end);
    next.is_none_or(|b| !b.is_ascii_alphanumeric() && *b != b'_')
        .then_some(end)
}

/// The end of ` <symbol>` right after an address, nested angle brackets
/// included, or `None` when no symbol follows.
fn symbol_end(bytes: &[u8], end: usize) -> Option<usize> {
    if !bytes[end..].starts_with(b" <") {
        return None;
    }
    let mut depth = 0usize;
    for (offset, &byte) in bytes[end + 1..].iter().enumerate() {
        match byte {
            b'<' => depth += 1,
            b'>' => {
                depth -= 1;
                if depth == 0 {
                    return Some(end + 1 + offset + 1);
                }
            }
            _ => {}
        }
    }
    None
}

/// Where a function's code lies: the offset of its entry and of each
/// loop's head from the start of a 64-byte line, which decides how many of
/// the loop's instructions one fetch brings. The same code at another
/// place can take another number of cycles: a loop whose head moved to the
/// end of a line starts each pass with a fetch that brings one useful
/// instruction.
#[derive(Debug, PartialEq, Eq)]
pub struct Place {
    pub entry: u64,
    /// Each loop, innermost (shortest) first: its head's offset and its
    /// instructions, from the head to the branch back to it.
    pub loops: Vec<(u64, usize)>,
}

/// The size of the line the offsets are taken in.
pub const LINE: u64 = 64;

/// The places of the functions of a listing whose names `wanted` keeps: a
/// loop is a branch to an earlier instruction of the same function.
pub fn places(listing: &str, wanted: impl Fn(&str) -> bool) -> BTreeMap<String, Vec<Place>> {
    /// A function being read: its name, entry and instructions with their addresses.
    struct Open {
        name: String,
        entry: u64,
        body: Vec<(u64, String)>,
    }
    let mut places: BTreeMap<String, Vec<Place>> = BTreeMap::new();
    let mut current: Option<Open> = None;
    let mut finish = |current: &mut Option<Open>| {
        if let Some(open) = current.take() {
            places
                .entry(open.name)
                .or_default()
                .push(place(open.entry, &open.body));
        }
    };
    for line in listing.lines() {
        if let Some(name) = header(line) {
            finish(&mut current);
            if wanted(&name) {
                let entry = line
                    .split_once(' ')
                    .and_then(|(address, _)| u64::from_str_radix(address, 16).ok())
                    .unwrap_or(0);
                current = Some(Open {
                    name,
                    entry,
                    body: Vec::new(),
                });
            }
        } else if let Some(Open { body, .. }) = current.as_mut()
            && let Some((address, rest)) = line.trim_start().split_once(':')
            && let Ok(address) = u64::from_str_radix(address, 16)
            && !rest.trim().is_empty()
        {
            body.push((address, rest.trim().to_owned()));
        }
    }
    finish(&mut current);
    places
}

fn place(entry: u64, body: &[(u64, String)]) -> Place {
    let mut loops = Vec::new();
    for (index, (address, text)) in body.iter().enumerate() {
        let Some(target) = branch_target(text) else {
            continue;
        };
        if target < entry || target > *address {
            continue;
        }
        let head = body[..=index].partition_point(|(at, _)| *at < target);
        loops.push((target % LINE, index - head + 1));
    }
    loops.sort_by_key(|&(_, length)| length);
    loops.dedup();
    Place {
        entry: entry % LINE,
        loops,
    }
}

/// The target of a jump or a conditional branch (not a call): the address
/// before objdump's ` <symbol>`.
fn branch_target(text: &str) -> Option<u64> {
    let mnemonic = text.split_ascii_whitespace().next()?;
    let branch = matches!(mnemonic, "b" | "cbz" | "cbnz" | "tbz" | "tbnz")
        || mnemonic.starts_with("b.")
        || mnemonic.starts_with('j');
    if !branch {
        return None;
    }
    let before = &text[..text.find(" <")?];
    let token = before.rsplit([' ', '\t', ',']).next()?;
    u64::from_str_radix(token.trim_start_matches("0x"), 16).ok()
}

/// What differs between two programs' functions.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Difference {
    pub only_before: BTreeSet<String>,
    pub only_after: BTreeSet<String>,
    pub changed: BTreeSet<String>,
    /// The changed functions whose instructions are the same but for
    /// their immediates: a field that moved within a struct, a constant
    /// that changed. The same work, at other offsets.
    pub immediates: BTreeSet<String>,
    /// The changed functions whose instructions are the same once the
    /// registers of one side are renamed to those of the other, one to
    /// one ([`registers_only`]).
    pub registers: BTreeSet<String>,
    pub same: usize,
}

impl Difference {
    pub fn is_empty(&self) -> bool {
        self.only_before.is_empty() && self.only_after.is_empty() && self.changed.is_empty()
    }
}

/// An instruction listing with its immediates masked: `#0x58`, `#-16` and
/// `$0x10` become `#N` and `$N`, a displacement before a register,
/// `0x10(%rsp)`, `N(%rsp)`.
fn without_immediates(body: &str) -> String {
    let bytes = body.as_bytes();
    let mut out = String::with_capacity(body.len());
    let mut at = 0;
    while at < bytes.len() {
        let byte = bytes[at];
        let number = |from: usize| {
            let mut end = from;
            if bytes.get(end) == Some(&b'-') {
                end += 1;
            }
            let digits = if bytes[end..].starts_with(b"0x") {
                end += 2;
                bytes[end..]
                    .iter()
                    .take_while(|b| b.is_ascii_hexdigit())
                    .count()
            } else {
                bytes[end..]
                    .iter()
                    .take_while(|b| b.is_ascii_digit())
                    .count()
            };
            (digits > 0).then_some(end + digits)
        };
        if (byte == b'#' || byte == b'$')
            && let Some(end) = number(at + 1)
        {
            out.push(char::from(byte));
            out.push('N');
            at = end;
            continue;
        }
        let starts_word =
            at == 0 || !bytes[at - 1].is_ascii_alphanumeric() && bytes[at - 1] != b'_';
        if starts_word
            && (byte == b'-' || byte.is_ascii_digit())
            && let Some(end) = number(at)
            && bytes.get(end) == Some(&b'(')
        {
            out.push('N');
            at = end;
            continue;
        }
        let length = body[at..].chars().next().map_or(1, char::len_utf8);
        out.push_str(&body[at..at + length]);
        at += length;
    }
    out
}

/// A register that a renaming may give another number: its bank, whose
/// registers can stand for one another, and its number in the bank. The
/// width it is read at (`x3` or `w3`, `%rax` or `%eax`) stays in the
/// instruction's text.
type Renamable = (char, u32);

/// A register named at some place of an operand: the text that stands for
/// it once its number is taken out, the register when a renaming may
/// change it (`None` for one with a fixed meaning, which stays as it is
/// written), and where its name ends.
type Named = (String, Option<Renamable>, usize);

/// Whether two bodies of a function do the same with other registers: the
/// instructions match one for one once each register a renaming may change
/// is mapped, one to one and in the same bank, to the register in its
/// place on the other side. A register and its narrower views count as
/// one.
///
/// The registers with a fixed meaning are never renamed: the stack and
/// frame pointers, the link and zero registers, the registers that carry
/// arguments and results, and those a linker or the platform may take. On
/// AArch64, where every instruction is four bytes, other numbers cost
/// nothing; on x86-64 a register past the first eight lengthens an
/// instruction, and so can move a loop within its line.
///
/// The bodies under one name (closures, instances) are paired in the order
/// of their text without registers.
pub fn registers_only(before: &[String], after: &[String]) -> bool {
    if before.len() != after.len() {
        return false;
    }
    let split = |bodies: &[String]| {
        let mut split: Vec<_> = bodies.iter().map(|body| renamable(body)).collect();
        split.sort();
        split
    };
    let (before, after) = (split(before), split(after));
    before
        .iter()
        .zip(&after)
        .all(|((text, from), (other, to))| {
            let mut forward = BTreeMap::new();
            let mut backward = BTreeMap::new();
            text == other
                && from.len() == to.len()
                && from.iter().zip(to).all(|(a, b)| {
                    a.0 == b.0
                        && *forward.entry(*a).or_insert(*b) == *b
                        && *backward.entry(*b).or_insert(*a) == *a
                })
        })
}

/// A body with each register a renaming may change taken out of its
/// operands: the text with the register's width and `@` in its place, and
/// the registers in the order they occur. Mnemonics, symbols and the
/// registers with a fixed meaning stay as they are.
fn renamable(body: &str) -> (String, Vec<Renamable>) {
    let mut text = String::with_capacity(body.len());
    let mut registers = Vec::new();
    for (index, line) in body.lines().enumerate() {
        if index > 0 {
            text.push('\n');
        }
        let Some((mnemonic, operands)) = line.split_once('\t') else {
            text.push_str(line);
            continue;
        };
        text.push_str(mnemonic);
        text.push('\t');
        let bytes = operands.as_bytes();
        let mut at = 0;
        while at < bytes.len() {
            if bytes[at] == b'<'
                && let Some(end) = matching(bytes, at)
            {
                text.push_str(&operands[at..end]);
                at = end;
                continue;
            }
            let starts_word = at == 0
                || !(bytes[at - 1].is_ascii_alphanumeric()
                    || matches!(bytes[at - 1], b'.' | b'#' | b'$' | b'_'));
            if starts_word
                && let Some((width, register, end)) =
                    arm_register(operands, at).or_else(|| x86_register(operands, at))
            {
                match register {
                    Some(register) => {
                        text.push_str(&width);
                        text.push('@');
                        registers.push(register);
                    }
                    None => text.push_str(&operands[at..end]),
                }
                at = end;
                continue;
            }
            let length = operands[at..].chars().next().map_or(1, char::len_utf8);
            text.push_str(&operands[at..at + length]);
            at += length;
        }
    }
    (text, registers)
}

/// An AArch64 register at `at`, numbered: `x` or `w` of the general bank,
/// `v`, `q`, `d`, `s`, `h` or `b` of the vector bank. Fixed: x0 to x7,
/// which carry arguments and results; x16 to x18, the linker's and the
/// platform's; x29 and x30, the frame pointer and the link register; v0
/// to v7, the vector arguments and results. `sp`, `wsp`, `xzr` and `wzr`
/// have no number and are never renamed. x8, which carries the address of
/// a large result into a call, is the compiler's first scratch register
/// elsewhere, and is renamed like the others.
fn arm_register(operands: &str, at: usize) -> Option<Named> {
    let bytes = operands.as_bytes();
    let (bank, last) = match bytes[at] {
        b'x' | b'w' => ('x', 30),
        b'v' | b'q' | b'd' | b's' | b'h' | b'b' => ('v', 31),
        _ => return None,
    };
    let digits = bytes[at + 1..]
        .iter()
        .take_while(|b| b.is_ascii_digit())
        .count();
    let end = at + 1 + digits;
    let joined = bytes
        .get(end)
        .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_');
    if digits == 0 || joined {
        return None;
    }
    let number: u32 = operands[at + 1..end].parse().ok().filter(|&n| n <= last)?;
    let fixed = match bank {
        'x' => number <= 7 || (16..=18).contains(&number) || number >= 29,
        _ => number <= 7,
    };
    let width = operands[at..at + 1].to_owned();
    Some((width, (!fixed).then_some((bank, number)), end))
}

/// An x86-64 register at `at`, `%` and its name, with the width it is read
/// at: `q`, `d`, `w`, `b` or `h` (the high byte) for the general bank,
/// `x`, `y` or `z` for the vector one. Fixed: the stack and frame
/// pointers and the registers of arguments and results, rax, rcx, rdx,
/// rsi, rdi, r8, r9 and the vectors 0 to 7. `%rip` and other names, such
/// as segments, are not taken as registers and stay as text.
fn x86_register(operands: &str, at: usize) -> Option<Named> {
    let bytes = operands.as_bytes();
    if bytes[at] != b'%' {
        return None;
    }
    let length = bytes[at + 1..]
        .iter()
        .take_while(|b| b.is_ascii_alphanumeric())
        .count();
    let end = at + 1 + length;
    let name = &operands[at + 1..end];
    if name.is_empty() {
        return None;
    }
    let (width, bank, number) = if let Some(vector) = ["xmm", "ymm", "zmm"]
        .iter()
        .find(|prefix| name.starts_with(**prefix))
    {
        let number: u32 = name[3..].parse().ok().filter(|&n| n < 32)?;
        (&vector[..1], 'v', number)
    } else if let Some(rest) = name.strip_prefix('r')
        && rest.starts_with(|c: char| c.is_ascii_digit())
    {
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        let number: u32 = rest[..digits]
            .parse()
            .ok()
            .filter(|n| (8..16).contains(n))?;
        let width = match &rest[digits..] {
            "" => "q",
            suffix @ ("d" | "w" | "b") => suffix,
            _ => return None,
        };
        (width, 'g', number)
    } else {
        // The first eight general registers by width, in the order of
        // their numbers.
        const LEGACY: [[&str; 5]; 8] = [
            ["rax", "eax", "ax", "al", "ah"],
            ["rcx", "ecx", "cx", "cl", "ch"],
            ["rdx", "edx", "dx", "dl", "dh"],
            ["rbx", "ebx", "bx", "bl", "bh"],
            ["rsp", "esp", "sp", "spl", ""],
            ["rbp", "ebp", "bp", "bpl", ""],
            ["rsi", "esi", "si", "sil", ""],
            ["rdi", "edi", "di", "dil", ""],
        ];
        const WIDTHS: [&str; 5] = ["q", "d", "w", "b", "h"];
        let (number, column) = LEGACY.iter().enumerate().find_map(|(number, names)| {
            let column = names.iter().position(|legacy| *legacy == name)?;
            Some((number, column))
        })?;
        (WIDTHS[column], 'g', u32::try_from(number).ok()?)
    };
    let fixed = match bank {
        'g' => ![3, 10, 11, 12, 13, 14, 15].contains(&number),
        _ => number <= 7,
    };
    let width = format!("%{width}");
    Some((width, (!fixed).then_some((bank, number)), end))
}

/// Compare two programs' functions name by name.
pub fn compare(before: &Functions, after: &Functions) -> Difference {
    let mut difference = Difference::default();
    for (name, bodies) in before {
        match after.get(name) {
            None => {
                difference.only_before.insert(name.clone());
            }
            Some(other) if other == bodies => difference.same += 1,
            Some(other) => {
                difference.changed.insert(name.clone());
                let shapes = |bodies: &[String]| -> Vec<String> {
                    bodies.iter().map(|body| without_immediates(body)).collect()
                };
                if shapes(other) == shapes(bodies) {
                    difference.immediates.insert(name.clone());
                }
                if registers_only(bodies, other) {
                    difference.registers.insert(name.clone());
                }
            }
        }
    }
    for name in after.keys() {
        if !before.contains_key(name) {
            difference.only_after.insert(name.clone());
        }
    }
    difference
}

#[cfg(test)]
mod tests {
    use super::*;

    const LISTING: &str = "\
filter_int32:\tfile format mach-o arm64

Disassembly of section __TEXT,__text:

000000010000088c <tessera_kernels::int::filter::filter_bulk::<i32, x<i32>>>:
10000088c:     \tsub\tsp, sp, #0x100
100000930:     \tb.eq\t0x100000ab8 <tessera_kernels::int::filter::filter_bulk::<i32, x<i32>>+0x22c>
100000974:     \tbl\t0x1000032a4 <tessera_kernels::int::filter::bulk_passing::h0123456789abcdef>
100000978:     \tadrp\tx8, 0x100004000
10000097c:     \tmov\tw2, #0x0                ; =0

0000000100001000 <core::fmt::write>:
100001000:     \tret

0000000100002000 <_tess_int4_filter>:
100002000:     \tret
";

    #[test]
    fn listings_keep_tessera_functions_without_addresses() {
        let functions = parse(LISTING);
        assert_eq!(
            functions.keys().collect::<Vec<_>>(),
            [
                "_tess_int4_filter",
                "tessera_kernels::int::filter::filter_bulk::<i32, x<i32>>"
            ]
        );
        assert_eq!(
            functions["tessera_kernels::int::filter::filter_bulk::<i32, x<i32>>"],
            ["sub\tsp, sp, #0x100\n\
              b.eq\t<tessera_kernels::int::filter::filter_bulk::<i32, x<i32>>+0x22c>\n\
              bl\t<tessera_kernels::int::filter::bulk_passing>\n\
              adrp\tx8, ADDR\n\
              mov\tw2, #0x0                ; =0"]
        );
    }

    #[test]
    fn moved_code_is_the_same_and_changed_code_is_not() {
        let before = parse(LISTING);
        let moved = LISTING
            .replace("10000088c", "20000088c")
            .replace("0x100000ab8", "0x200000ab8")
            .replace("0x100004000", "0x200008000")
            .replace("h0123456789abcdef", "hfedcba9876543210");
        let difference = compare(&before, &parse(&moved));
        assert!(difference.is_empty(), "{difference:?}");
        assert_eq!(difference.same, 2);
        let changed = LISTING.replace("#0x100", "#0x110");
        let difference = compare(&before, &parse(&changed));
        assert_eq!(difference.changed.len(), 1);
        let renamed = LISTING.replace("_tess_int4_filter", "_tess_int4_filter2");
        let difference = compare(&before, &parse(&renamed));
        assert_eq!(
            difference.only_before,
            BTreeSet::from(["_tess_int4_filter".to_owned()])
        );
        assert_eq!(
            difference.only_after,
            BTreeSet::from(["_tess_int4_filter2".to_owned()])
        );
    }

    #[test]
    fn data_pages_and_their_offsets_are_addresses() {
        let mut pages = Pages::default();
        let lines = [
            "adrp\tx10, <dyld_stub_binder+0x100052000>",
            "ldr\tq2, [x10, #0x720]",
            "add\tx1, x10, #0x18",
            "ldr\tx8, [x8, #0x10]",
            "mov\tx10, x0",
            "ldr\tq2, [x10, #0x720]",
            "ldr\tx3, 0x100004000 <sym>",
            "str\tx10, [sp, #0x8]",
        ];
        let normalized: Vec<_> = lines
            .iter()
            .map(|line| pages.normalize(&normalize(line)))
            .collect();
        assert_eq!(
            normalized,
            [
                "adrp\tx10, ADDR",
                "ldr\tq2, [x10, LO12]",
                "add\tx1, x10, LO12",
                "ldr\tx8, [x8, #0x10]",
                "mov\tx10, x0",
                "ldr\tq2, [x10, #0x720]",
                "ldr\tx3, ADDR",
                "str\tx10, [sp, #0x8]",
            ]
        );
        assert_eq!(
            Pages::default().normalize("lea    0x2f0e(%rip),%rax        # 405000 <sym>"),
            "lea    0x2f0e(%rip),%rax"
        );
    }

    #[test]
    fn offsets_into_other_symbols_move() {
        let function = "tessera_kernels::f";
        assert_eq!(
            foreign_offsets("bl\t<dyld_stub_binder+0x4f3e0>", function),
            "bl\t<dyld_stub_binder+OFF>"
        );
        assert_eq!(
            foreign_offsets("b.eq\t<tessera_kernels::f+0x22c>", function),
            "b.eq\t<tessera_kernels::f+0x22c>"
        );
        assert_eq!(
            foreign_offsets("bl\t<tessera_kernels::g::<i32, x<i32>>>", function),
            "bl\t<tessera_kernels::g::<i32, x<i32>>>"
        );
    }

    #[test]
    fn loops_and_entries_lie_at_offsets_in_a_line() {
        let listing = "\
0000000000004000 <_agg_exec>:
    4000:\tret

00000000000043c8 <_agg_generic_accumulate>:
    43c8:\tstp\tx29, x30, [sp, #-0x10]!
    43cc:\tmov\tx19, x0
    43d0:\tadd\tx19, x19, #0x1
    43d4:\tbl\t0x4000 <_agg_exec>
    43d8:\tcmp\tx19, #0x40
    43dc:\tb.ne\t0x43d0 <_agg_generic_accumulate+0x8>
    43e0:\tcbnz\tx19, 0x43cc <_agg_generic_accumulate+0x4>
    43e4:\tb\t0x4000 <_agg_exec>
";
        let places = places(listing, |name| name.contains("accumulate"));
        assert_eq!(
            places.keys().collect::<Vec<_>>(),
            ["_agg_generic_accumulate"]
        );
        assert_eq!(
            places["_agg_generic_accumulate"],
            [Place {
                entry: 8,
                loops: vec![(16, 4), (12, 6)]
            }]
        );
        assert_eq!(branch_target("jne    401020 <f+0x20>"), Some(0x401020));
        assert_eq!(branch_target("call   401020 <g>"), None);
        assert_eq!(branch_target("bl\t0x4000 <g>"), None);
    }

    #[test]
    fn pieces_cut_out_of_functions_lose_their_numbers() {
        assert_eq!(fragments("_OUTLINED_FUNCTION_12"), "_OUTLINED_FUNCTION_N");
        assert_eq!(fragments("_agg_begin.cold.3"), "_agg_begin.cold.N");
        assert_eq!(fragments("_agg_begin"), "_agg_begin");
        assert_eq!(
            normalize("bl\t0x1000 <_OUTLINED_FUNCTION_7>"),
            "bl\t<_OUTLINED_FUNCTION_N>"
        );
        let before = "\
0000000000004000 <_f.cold.1>:
    4000:\tret
0000000000004004 <_f.cold.2>:
    4004:\tnop
";
        let after = before
            .replace("cold.1", "cold.9")
            .replace("cold.2", "cold.1");
        assert!(compare(&parse_all(before), &parse_all(&after)).is_empty());
        assert!(fragment("_f.cold.N") && !fragment("_f"));
    }

    #[test]
    fn data_at_the_start_of_a_page_has_an_offset_too() {
        let mut pages = Pages::default();
        assert_eq!(pages.normalize("adrp\tx8, ADDR"), "adrp\tx8, ADDR");
        assert_eq!(pages.normalize("ldr\tq1, [x8]"), "ldr\tq1, [x8, LO12]");
    }

    #[test]
    fn a_moved_field_changes_immediates_only() {
        let listing = "\
0000000000004000 <_f>:
    4000:\tldr\tx8, [x0, #0x58]
    4004:\tadd\tx8, x8, #16
    4008:\tret
";
        let before = parse_all(listing);
        let moved = parse_all(&listing.replace("#0x58", "#0x60").replace("#16", "#24"));
        let difference = compare(&before, &moved);
        assert_eq!(difference.changed.len(), 1);
        assert_eq!(difference.immediates.len(), 1, "the same instructions");
        let other = parse_all(&listing.replace("add\tx8, x8, #16", "sub\tx8, x8, #16"));
        assert!(compare(&before, &other).immediates.is_empty());
        assert_eq!(
            without_immediates("mov    0x10(%rsp),%rax\nmov    $0x100,%eax\nldr\tx8, [x0, #-8]"),
            "mov    N(%rsp),%rax\nmov    $N,%eax\nldr\tx8, [x0, #N]"
        );
    }

    #[test]
    fn a_library_keeps_every_function() {
        let listing = "\
0000000000004000 <_agg_exec>:
    4000:\tret
";
        assert!(parse(listing).is_empty());
        assert_eq!(parse_all(listing)["_agg_exec"], ["ret"]);
    }

    #[test]
    fn gnu_branch_targets_and_immediates() {
        assert_eq!(
            normalize("call   401020 <tess_int4_hash>"),
            "call   <tess_int4_hash>"
        );
        assert_eq!(normalize("mov    $0x100,%eax"), "mov    $0x100,%eax");
        assert_eq!(
            normalize("mov    0x10(%rsp),%rax"),
            "mov    0x10(%rsp),%rax"
        );
        assert_eq!(
            normalize("lea    0x2f0e(%rip),%rax"),
            "lea    ADDR(%rip),%rax"
        );
        assert_eq!(normalize("add    x0, x1, #0x40"), "add    x0, x1, #0x40");
        assert_eq!(
            normalize("ldr    x8, [x8, #0x10]"),
            "ldr    x8, [x8, #0x10]"
        );
        assert_eq!(strip_hashes("a::b::h0123456789abcdef"), "a::b");
        assert_eq!(strip_hashes("a::hello"), "a::hello");
    }
    #[test]
    fn renamed_registers_on_aarch64_are_told_apart() {
        let body = |text: &str| vec![text.replace(';', "\n")];
        let before = body(
            "ldr\tq16, [x0, x9, lsl #4];cmgt\tv16.4s, v16.4s, v17.4s;\
             add\tx9, x9, #0x1;str\tw10, [x1];mov\tw9, w10",
        );
        let renamed = body(
            "ldr\tq20, [x0, x11, lsl #4];cmgt\tv20.4s, v20.4s, v18.4s;\
             add\tx11, x11, #0x1;str\tw12, [x1];mov\tw11, w12",
        );
        assert!(registers_only(&before, &renamed));
        let two_to_one = body(
            "ldr\tq20, [x0, x11, lsl #4];cmgt\tv20.4s, v20.4s, v20.4s;\
             add\tx11, x11, #0x1;str\tw12, [x1];mov\tw11, w12",
        );
        assert!(!registers_only(&before, &two_to_one));
        let one_to_two = body(
            "ldr\tq20, [x0, x11, lsl #4];cmgt\tv21.4s, v20.4s, v18.4s;\
             add\tx11, x11, #0x1;str\tw12, [x1];mov\tw11, w12",
        );
        assert!(!registers_only(&before, &one_to_two));
        let argument = body(
            "ldr\tq16, [x2, x9, lsl #4];cmgt\tv16.4s, v16.4s, v17.4s;\
             add\tx9, x9, #0x1;str\tw10, [x1];mov\tw9, w10",
        );
        assert!(
            !registers_only(&before, &argument),
            "x0 carries an argument"
        );
        let stack = body(
            "ldr\tq16, [sp, x9, lsl #4];cmgt\tv16.4s, v16.4s, v17.4s;\
             add\tx9, x9, #0x1;str\tw10, [x1];mov\tw9, w10",
        );
        assert!(!registers_only(&before, &stack));
        let lanes = body(
            "ldr\tq16, [x0, x9, lsl #4];cmgt\tv16.2d, v16.2d, v17.2d;\
             add\tx9, x9, #0x1;str\tw10, [x1];mov\tw9, w10",
        );
        assert!(!registers_only(&before, &lanes), "the arrangement is kept");
        let other = body(
            "ldr\tq16, [x0, x9, lsl #4];cmge\tv16.4s, v16.4s, v17.4s;\
             add\tx9, x9, #0x1;str\tw10, [x1];mov\tw9, w10",
        );
        assert!(!registers_only(&before, &other));
        let symbols = body("bl\t<tessera::x9::v16>;mov\tx9, x10");
        assert!(registers_only(
            &symbols,
            &body("bl\t<tessera::x9::v16>;mov\tx11, x12")
        ));
        assert!(!registers_only(
            &symbols,
            &body("bl\t<tessera::x11::v16>;mov\tx11, x12")
        ));
    }

    #[test]
    fn renamed_registers_on_x86_64_are_told_apart() {
        let body = |text: &str| vec![text.replace(';', "\n")];
        let before = body(
            "vmovdqu\t(%rdi,%r10), %ymm8;vpcmpgtd\t%ymm9, %ymm8, %ymm10;\
             movl\t%r10d, %ebx;addq\t$0x20, %r10;movb\t%bl, 0x8(%rsp)",
        );
        let renamed = body(
            "vmovdqu\t(%rdi,%r11), %ymm11;vpcmpgtd\t%ymm12, %ymm11, %ymm13;\
             movl\t%r11d, %r12d;addq\t$0x20, %r11;movb\t%r12b, 0x8(%rsp)",
        );
        assert!(registers_only(&before, &renamed));
        let argument = body(
            "vmovdqu\t(%rsi,%r10), %ymm8;vpcmpgtd\t%ymm9, %ymm8, %ymm10;\
             movl\t%r10d, %ebx;addq\t$0x20, %r10;movb\t%bl, 0x8(%rsp)",
        );
        assert!(
            !registers_only(&before, &argument),
            "rdi carries an argument"
        );
        let width = body(
            "vmovdqu\t(%rdi,%r11), %ymm11;vpcmpgtd\t%ymm12, %ymm11, %ymm13;\
             movl\t%r11d, %r12d;addq\t$0x20, %r11;movw\t%r12w, 0x8(%rsp)",
        );
        assert!(!registers_only(&before, &width));
        let stack = body(
            "vmovdqu\t(%rdi,%r11), %ymm11;vpcmpgtd\t%ymm12, %ymm11, %ymm13;\
             movl\t%r11d, %r12d;addq\t$0x20, %r11;movb\t%r12b, 0x8(%rbp)",
        );
        assert!(!registers_only(&before, &stack));
        let two_to_one = body(
            "vmovdqu\t(%rdi,%r11), %ymm11;vpcmpgtd\t%ymm11, %ymm11, %ymm13;\
             movl\t%r11d, %r12d;addq\t$0x20, %r11;movb\t%r12b, 0x8(%rsp)",
        );
        assert!(!registers_only(&before, &two_to_one));
    }

    #[test]
    fn a_comparison_marks_registers_only() {
        let before = parse(LISTING);
        let renamed = LISTING.replace("x8, 0x100004000", "x9, 0x100004000");
        let difference = compare(&before, &parse(&renamed));
        let name = "tessera_kernels::int::filter::filter_bulk::<i32, x<i32>>";
        assert_eq!(difference.changed, BTreeSet::from([name.to_owned()]));
        assert_eq!(difference.registers, BTreeSet::from([name.to_owned()]));
        let changed = LISTING.replace("#0x100", "#0x110");
        assert!(compare(&before, &parse(&changed)).registers.is_empty());
    }
}
