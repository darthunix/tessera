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
//! absolute addresses become `ADDR`. Registers, immediates and the order
//! of the instructions are kept: with one codegen unit the same source
//! gives the same code.

use anyhow::{Context, Result, ensure};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    process::Command,
};

/// A program's functions: per name, the bodies under that name (closures
/// and generic instances may share one after normalization).
pub type Functions = BTreeMap<String, Vec<String>>;

/// The functions of a program that belong to Tessera, normalized.
pub fn functions(executable: &Path) -> Result<Functions> {
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
    Ok(parse(&String::from_utf8_lossy(&output.stdout)))
}

/// Whether a function is Tessera's: its path or an instance's type
/// mentions a Tessera crate, or it is an entry point of the C API.
fn tessera(name: &str) -> bool {
    name.contains("tessera") || name.trim_start_matches('_').starts_with("tess_")
}

/// Parse objdump's listing into Tessera's functions.
pub fn parse(listing: &str) -> Functions {
    let mut functions = Functions::new();
    let mut current: Option<(String, Vec<String>)> = None;
    let mut finish = |current: &mut Option<(String, Vec<String>)>| {
        if let Some((name, body)) = current.take() {
            functions.entry(name).or_default().push(body.join("\n"));
        }
    };
    for line in listing.lines() {
        if let Some(name) = header(line) {
            finish(&mut current);
            if tessera(&name) {
                current = Some((name, Vec::new()));
            }
        } else if let Some((_, body)) = current.as_mut()
            && let Some(instruction) = instruction(line)
        {
            body.push(instruction);
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
    Some(strip_hashes(name))
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
                out.push_str(&strip_hashes(&text[end + 2..symbol_end - 1]));
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

/// What differs between two programs' functions.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Difference {
    pub only_before: BTreeSet<String>,
    pub only_after: BTreeSet<String>,
    pub changed: BTreeSet<String>,
    pub same: usize,
}

impl Difference {
    pub fn is_empty(&self) -> bool {
        self.only_before.is_empty() && self.only_after.is_empty() && self.changed.is_empty()
    }
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
            Some(_) => {
                difference.changed.insert(name.clone());
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
}
