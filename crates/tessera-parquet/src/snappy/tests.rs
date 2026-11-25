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
        let mut out = vec![0; decompress_len(input).ok()?];
        let n = decompress(input, &mut out).ok()?;
        assert_eq!(n, out.len());
        return Some(out);
    }
    let len = snap::raw::decompress_len(input).ok()?;
    if len > input.len() * 22 {
        return None;
    }
    let mut out = vec![0; len];
    let n = snap::raw::Decoder::new().decompress(input, &mut out).ok()?;
    Some(out[..n].to_vec())
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
                .chain([31, 32, 33, 63, 64, 65, 180, 181, 300])
                .step_by(sized(1, 4))
            {
                let mut elements = literals(3);
                elements.push(Element::Copy { len, offset, width });
                elements.extend(literals(4));
                let out = same(&stream(written(&elements), &elements));
                assert_eq!(
                    out.is_some(),
                    (1..=180).contains(&offset),
                    "{width} {len} {offset}"
                );
            }
        }
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
