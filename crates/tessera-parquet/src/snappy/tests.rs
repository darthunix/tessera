use proptest::prelude::*;

use super::*;

/// `n`, or `small` under Miri, which runs each case thousands of times
/// slower.
fn sized(n: usize, small: usize) -> usize {
    if cfg!(miri) { small } else { n }
}

fn compress(data: &[u8]) -> Vec<u8> {
    snap::raw::Encoder::new().compress_vec(data).unwrap()
}

/// The bytes a decoder gives for `input`, or `None` for an error: this
/// module's, or `snap`'s, into an output of the length its header gives.
/// A length that no stream of `input`'s size can reach is `snap`'s error
/// too, without allocating it.
fn decode(input: &[u8], mine: bool) -> Option<Vec<u8>> {
    if mine {
        return fenced(input);
    }
    let len = snap::raw::decompress_len(input).ok()?;
    if len > input.len() * 22 {
        return None;
    }
    let mut out = vec![0; len];
    let n = snap::raw::Decoder::new().decompress(input, &mut out).ok()?;
    Some(out[..n].to_vec())
}

/// The bytes on either side of a buffer of the decoder, which it must not
/// touch.
const FENCE: usize = 256;

/// This module's bytes for `input`, decoded from a copy followed by
/// literals of one byte, which a loop reading past the input's end would
/// take, into an output between bytes 0xA5, which a write past either end
/// would change: none must.
fn fenced(input: &[u8]) -> Option<Vec<u8>> {
    let len = decompress_len(input).ok()?;
    let mut from = input.to_vec();
    from.resize(input.len() + FENCE, 0);
    let mut to = vec![0xa5; FENCE + len + FENCE];
    let done = decompress(&from[..input.len()], &mut to[FENCE..FENCE + len]);
    assert!(
        to[..FENCE]
            .iter()
            .chain(&to[FENCE + len..])
            .all(|&b| b == 0xa5),
        "a write past the output of {input:?}"
    );
    assert_eq!(done.ok()?, len);
    Some(to[FENCE..FENCE + len].to_vec())
}

/// Both decoders give the same bytes for `input`, or both an error; the
/// bytes, when they agree on them.
fn same(input: &[u8]) -> Option<Vec<u8>> {
    let mine = decode(input, true);
    assert_eq!(
        mine,
        decode(input, false),
        "the decoders differ on {input:?}"
    );
    mine
}

/// Text of repeated parts, as the pages of strings hold.
fn addresses(rows: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for i in 0..rows {
        let site = ["example.com", "yandex.ru", "a.b"][i % 3];
        out.extend_from_slice(format!("http://{site}/page/{}?q={}", i % 17, i * 7919).as_bytes());
    }
    out
}

/// An element of a raw stream, written by hand.
#[derive(Debug, Clone)]
enum Element {
    /// A literal, its length in `extra` bytes after the tag when that is
    /// not 0, in the tag otherwise.
    Literal { bytes: Vec<u8>, extra: usize },
    /// A copy with `width` bytes of offset: 1, 2 or 4.
    Copy {
        len: usize,
        offset: usize,
        width: usize,
    },
}

/// The stream of `elements` whose header says `len`.
fn stream(len: usize, elements: &[Element]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut n = len as u64;
    while n >= 0x80 {
        out.push(n as u8 | 0x80);
        n >>= 7;
    }
    out.push(n as u8);
    for e in elements {
        match e {
            Element::Literal { bytes, extra: 0 } => {
                out.push(((bytes.len() - 1) << 2) as u8);
                out.extend_from_slice(bytes);
            }
            Element::Literal { bytes, extra } => {
                out.push(((59 + extra) << 2) as u8);
                out.extend_from_slice(&(bytes.len() as u32 - 1).to_le_bytes()[..*extra]);
                out.extend_from_slice(bytes);
            }
            Element::Copy {
                len,
                offset,
                width: 1,
            } => {
                out.push((((offset >> 8) << 5) | ((len - 4) << 2) | 1) as u8);
                out.push(*offset as u8);
            }
            Element::Copy { len, offset, width } => {
                out.push((((len - 1) << 2) | if *width == 2 { 2 } else { 3 }) as u8);
                out.extend_from_slice(&(*offset as u32).to_le_bytes()[..*width]);
            }
        }
    }
    out
}

/// The length the elements write when every one is valid.
fn written(elements: &[Element]) -> usize {
    elements
        .iter()
        .map(|e| match e {
            Element::Literal { bytes, .. } => bytes.len(),
            Element::Copy { len, .. } => *len,
        })
        .sum()
}

fn bytes(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i * 37 + 11) as u8).collect()
}

/// A literal of `n` bytes, its length in the tag up to 60, in one byte
/// after it past that.
fn literal(n: usize) -> Element {
    Element::Literal {
        bytes: bytes(n),
        extra: usize::from(n > 60),
    }
}

#[test]
fn a_stream_decodes_to_its_bytes() {
    let random: Vec<u8> = (0..sized(70_000, 700) as u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect();
    for data in [
        Vec::new(),
        b"a".to_vec(),
        vec![7; sized(100_000, 1_000)],
        addresses(sized(5_000, 50)),
        random,
    ] {
        assert_eq!(same(&compress(&data)), Some(data));
    }
}

#[test]
fn a_damaged_stream_says_what_failed() {
    assert_eq!(
        damaged("its header").to_string(),
        "damaged Snappy stream: its header"
    );
    // A length of 22 times the stream's size is let through to its
    // elements, one past that is refused before any is read.
    assert_eq!(decompress_len(&[44, 0]), Ok(44));
    assert_eq!(
        decompress_len(&[45, 0]),
        Err(damaged("a length past what its elements can write"))
    );
    assert_eq!(
        decompress(&[44, 0], &mut [0; 44]),
        Err(damaged("a literal past its end"))
    );
}

#[test]
fn a_header_is_refused_as_snap_refuses_it() {
    for input in [
        &[][..],
        &[0x80],
        &[0xff, 0xff, 0xff, 0xff, 0x0f],
        &[0xff, 0xff, 0xff, 0xff, 0x10],
        &[0x80, 0x80, 0x80, 0x80, 0x80, 0x00],
        &[
            0x0a, 0x24, b'0', b'1', b'2', b'3', b'4', b'5', b'6', b'7', b'8', b'9',
        ],
    ] {
        same(input);
    }
    assert_eq!(
        decompress_len(&[0xff, 0xff, 0xff, 0xff, 0x0f]),
        Err(damaged("a length past what its elements can write"))
    );
    assert!(decompress(&compress(b"0123456789"), &mut [0; 9]).is_err());
    assert!(decompress(&compress(b"0123456789"), &mut [0; 11]).is_err());
}

#[test]
fn every_copy_decodes_as_the_reference_decodes() {
    for width in [1, 2, 4] {
        let lens = if width == 1 { 4..=11 } else { 1..=64 };
        for len in lens {
            for offset in (0..=20)
                .chain([63, 64, 65, 70, 71, 300])
                .step_by(sized(1, 3))
            {
                let elements = [literal(70), Element::Copy { len, offset, width }];
                let out = same(&stream(written(&elements), &elements));
                assert_eq!(
                    out.is_some(),
                    (1..=70).contains(&offset),
                    "{width} {len} {offset}"
                );
            }
        }
    }
}

#[test]
fn every_literal_decodes_as_the_reference_decodes() {
    for n in (1..=70)
        .chain([255, 256, 257])
        .chain([65_536, 65_537].into_iter().filter(|_| !cfg!(miri)))
    {
        for extra in 0..=4 {
            let fits = match extra {
                0 => n <= 60,
                1 => n <= 256,
                2 => n <= 65_536,
                _ => true,
            };
            if fits {
                let elements = [Element::Literal {
                    bytes: bytes(n),
                    extra,
                }];
                assert_eq!(same(&stream(n, &elements)), Some(bytes(n)), "{n} {extra}");
            }
        }
    }
}

#[test]
fn a_stream_cut_or_lengthened_decodes_as_the_reference_decodes() {
    let data = addresses(sized(300, 20));
    let input = compress(&data);
    for cut in 0..input.len() {
        same(&input[..cut]);
    }
    let mut longer = input.clone();
    longer.extend_from_slice(&[0, b'x']);
    assert_eq!(same(&longer), None);
    let skip = input.iter().position(|&b| b < 0x80).unwrap() + 1;
    for len in [data.len() - 1, data.len() + 1, 0, u32::MAX as usize] {
        let mut other = stream(len, &[]);
        other.extend_from_slice(&input[skip..]);
        assert_eq!(same(&other), None, "{len}");
    }
}

fn element() -> impl Strategy<Value = Element> {
    prop_oneof![
        (1usize..80, 0usize..=4).prop_map(|(n, extra)| Element::Literal {
            bytes: bytes(n),
            extra: if n > 60 && extra == 0 { 1 } else { extra },
        }),
        (4usize..=11, 0usize..300).prop_map(|(len, offset)| Element::Copy {
            len,
            offset,
            width: 1
        }),
        (1usize..=64, 0usize..300, prop::sample::select(vec![2, 4]))
            .prop_map(|(len, offset, width)| Element::Copy { len, offset, width }),
    ]
}

proptest! {
    #[test]
    fn any_bytes_decode_as_the_reference_decodes(
        input in proptest::collection::vec(any::<u8>(), 0..300),
    ) {
        same(&input);
    }

    #[test]
    fn a_changed_stream_decodes_as_the_reference_decodes(
        rows in 1usize..200,
        at in any::<proptest::sample::Index>(),
        byte in any::<u8>(),
    ) {
        let mut input = compress(&addresses(rows));
        let at = at.index(input.len());
        input[at] = byte;
        same(&input);
    }

    #[test]
    fn any_elements_decode_as_the_reference_decodes(
        elements in proptest::collection::vec(element(), 0..40),
        slack in -2isize..=2,
    ) {
        let len = (written(&elements) as isize + slack).max(0) as usize;
        same(&stream(len, &elements));
    }
}

/// `count` literals of 60 bytes: input for the fast loop on either side
/// of an element.
fn literals(count: usize) -> Vec<Element> {
    (0..count).map(|_| literal(60)).collect()
}

#[test]
fn every_copy_inside_the_fast_loop_decodes_as_the_reference_decodes() {
    for width in [1, 2, 4] {
        let lens = if width == 1 { 4..=11 } else { 1..=64 };
        for len in lens {
            for offset in (0..=20)
                .chain([31, 32, 33, 63, 64, 65, 255, 256, 257, 300, 301])
                .step_by(sized(1, 4))
            {
                // Past the first 256 bytes, where literals and copies
                // both stay in the fast loop.
                let mut elements = literals(5);
                elements.push(Element::Copy { len, offset, width });
                elements.extend(literals(4));
                let out = same(&stream(written(&elements), &elements));
                assert_eq!(
                    out.is_some(),
                    (1..=300).contains(&offset),
                    "{width} {len} {offset}"
                );
            }
        }
    }
}

#[test]
fn the_fast_loop_decodes_short_literals_and_copies_itself() {
    // Literals of up to 60 bytes and copies of 1 and 2 bytes of offset,
    // past the first 256 bytes: the fast loop takes them all and stops
    // only near the ends, leaving the careful loop no more than the last
    // 130 bytes of input or 128 of output.
    let mut elements = literals(5);
    for i in 0..sized(200, 20) {
        elements.push(literal(1 + i % 60));
        elements.push(Element::Copy {
            len: 4 + i % 8,
            offset: 1 + i % 200,
            width: 1,
        });
        elements.push(Element::Copy {
            len: 1 + i % 64,
            offset: 1 + i % 280,
            width: 2,
        });
    }
    let input = stream(written(&elements), &elements);
    let (len, start) = header(&input).unwrap();
    let body = &input[start..];
    let mut out = vec![0; len];
    let (ip, op) = fast(body, 0, &mut out, 0);
    assert!(
        ip >= body.len() - 130 - 61 || op >= len - 128 - 64,
        "{ip} of {}, {op} of {len}",
        body.len()
    );
    assert_eq!(out[..op], same(&input).unwrap()[..op]);
}

#[test]
fn the_fast_loop_starts_only_with_its_room() {
    // Zero bytes are literals of one byte each, which the fast loop takes
    // wherever it starts.
    let input = [0u8; 300];
    let mut out = [0u8; 300];
    // 130 bytes of input with the tag, or 127 of output past the
    // frontier, and the loop does not start; 131 and 128, and it does.
    assert_eq!(fast(&input, 300 - 130, &mut out, 0), (300 - 130, 0));
    assert!(fast(&input, 300 - 131, &mut out, 0).0 > 300 - 131);
    assert_eq!(fast(&input, 0, &mut out, 300 - 127), (0, 300 - 127));
    assert!(fast(&input, 0, &mut out, 300 - 128).1 > 300 - 128);
    assert_eq!(fast(&input, 0, &mut out[..127], 0), (0, 0));
    assert!(fast(&input, 0, &mut out[..128], 0).1 > 0);
    // A turn decodes two literals, 4 bytes of input and 2 of output; the
    // next starts only with its room still there: 130 bytes of input past
    // its tag and 128 of output past the frontier and the copy that
    // waits. So one turn, or two, at each side's edge.
    assert_eq!(fast(&input[..134], 0, &mut out, 0), (4, 2));
    assert_eq!(fast(&input[..135], 0, &mut out, 0), (8, 4));
    assert_eq!(fast(&input, 0, &mut out[..129], 0), (4, 2));
    assert_eq!(fast(&input, 0, &mut out[..130], 0), (8, 4));
}

#[test]
fn the_fast_loop_stops_where_its_output_ends_first() {
    // Literals of one byte take two bytes of input for one of output, so
    // the output's guard stops the loop while input remains, here and
    // with any short header.
    let elements: Vec<Element> = (0..sized(600, 200)).map(|_| literal(1)).collect();
    for less in [0, 1, 100] {
        let len = written(&elements) - less;
        assert_eq!(same(&stream(len, &elements)).is_some(), less == 0);
    }
}

#[test]
fn the_fast_loop_stops_at_each_place_near_the_ends() {
    // A copy and a literal at every place of a stream around the fast
    // loop's guards: 130 bytes of input left, 128 of output.
    for prefix in (0..=200).step_by(sized(1, 13)) {
        let mut elements = vec![literal(prefix.max(1))];
        elements.extend([
            Element::Copy {
                len: 64,
                offset: 1,
                width: 2,
            },
            literal(5),
            Element::Copy {
                len: 11,
                offset: 3,
                width: 1,
            },
            Element::Copy {
                len: 40,
                offset: 20,
                width: 2,
            },
            literal(60),
            literal(61),
            Element::Copy {
                len: 7,
                offset: 9,
                width: 4,
            },
        ]);
        for slack in [0, 1] {
            let len = written(&elements) + slack;
            assert_eq!(
                same(&stream(len, &elements)).is_some(),
                slack == 0,
                "{prefix}"
            );
        }
    }
}
