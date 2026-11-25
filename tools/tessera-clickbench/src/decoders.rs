//! `cargo clickbench decoders`: the reader's decoder of Snappy,
//! `tessera_parquet::snappy`, against the crate `snap`'s, on the
//! compressed pages of a file's columns. The counters need root: build
//! the tool, then run it with `sudo -n` (see the README).

use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use parquet::basic::Compression;
use parquet::column::page::Page;
use parquet::file::metadata::{ParquetMetaData, ParquetMetaDataReader};
use parquet::file::serialized_reader::SerializedPageReader;

/// The columns counted by default: four of strings, then numbers.
pub const COLUMNS: &str = "URL,Title,Referer,SearchPhrase,EventTime,CounterID,UserID,WatchID";

/// The compressed pages of one column.
pub struct Pages {
    pub column: String,
    /// Each page's compressed stream and its decompressed length.
    pub pages: Vec<(Vec<u8>, usize)>,
    pub input: usize,
    pub output: usize,
}

/// About `budget` bytes of the compressed pages of each column, taken
/// from groups of rows spread over the file.
pub fn collect(path: &Path, columns: &[String], budget: usize) -> Result<Vec<Pages>> {
    let file =
        Arc::new(File::open(path).with_context(|| format!("cannot open {}", path.display()))?);
    let meta = ParquetMetaDataReader::new().parse_and_finish(file.as_ref())?;
    columns
        .iter()
        .map(|name| column(&file, &meta, name, budget))
        .collect()
}

fn column(file: &Arc<File>, meta: &ParquetMetaData, name: &str, budget: usize) -> Result<Pages> {
    let schema = meta.file_metadata().schema_descr();
    let Some(leaf) = (0..schema.num_columns()).find(|&i| schema.column(i).name() == name) else {
        bail!("the file has no column {name}");
    };
    let groups = meta.num_row_groups();
    let total: usize = (0..groups)
        .map(|g| meta.row_group(g).column(leaf).compressed_size() as usize)
        .sum();
    let take = (budget.max(1) * groups)
        .div_ceil(total.max(1))
        .clamp(1, groups.max(1));
    let mut out = Pages {
        column: name.into(),
        pages: Vec::new(),
        input: 0,
        output: 0,
    };
    for g in (0..groups).step_by((groups / take).max(1)) {
        if out.input >= budget {
            break;
        }
        let group = meta.row_group(g);
        let chunk = group.column(leaf);
        if chunk.compression() != Compression::SNAPPY {
            bail!(
                "{name} is compressed with {:?}, not Snappy",
                chunk.compression()
            );
        }
        // Marked uncompressed, the crate's page reader hands the pages
        // over as the file holds them.
        let raw = chunk
            .clone()
            .into_builder()
            .set_compression(Compression::UNCOMPRESSED)
            .build()?;
        let rows = usize::try_from(group.num_rows())?;
        for page in SerializedPageReader::new(file.clone(), &raw, rows, None)? {
            let stream = match page? {
                Page::DictionaryPage { buf, .. } | Page::DataPage { buf, .. } => buf,
                Page::DataPageV2 {
                    buf,
                    def_levels_byte_len,
                    rep_levels_byte_len,
                    is_compressed,
                    ..
                } => {
                    if !is_compressed {
                        continue;
                    }
                    buf.slice(usize::try_from(def_levels_byte_len + rep_levels_byte_len)?..)
                }
            };
            if stream.is_empty() {
                continue;
            }
            let len = snap::raw::decompress_len(&stream)?;
            out.input += stream.len();
            out.output += len;
            out.pages.push((stream.to_vec(), len));
        }
    }
    Ok(out)
}

/// Both decoders on every page: an error when they give different
/// bytes or one of them fails.
pub fn check(columns: &[Pages]) -> Result<()> {
    let max = columns
        .iter()
        .flat_map(|c| c.pages.iter().map(|p| p.1))
        .max()
        .unwrap_or(0);
    let (mut mine, mut theirs) = (vec![0; max], vec![0; max]);
    let mut reference = snap::raw::Decoder::new();
    for c in columns {
        for (i, (page, len)) in c.pages.iter().enumerate() {
            let a = tessera_parquet::snappy::decompress(page, &mut mine[..*len]);
            let b = reference.decompress(page, &mut theirs[..*len]);
            match (a, b) {
                (Ok(a), Ok(b)) if a == b && mine[..a] == theirs[..b] => {}
                (a, b) => bail!(
                    "{}, page {i}: the decoders differ: {a:?} and {b:?}",
                    c.column
                ),
            }
        }
    }
    Ok(())
}

/// The counts of one decoder over all pages of a column, per byte
/// decompressed: the medians of the passes.
#[derive(Debug, Clone, Copy)]
pub struct PerByte {
    pub instructions: f64,
    pub cycles: f64,
    pub branches: f64,
    pub misses: f64,
}

/// Each decoder over all pages of each column `passes` times, the two
/// in turns, in the order flipped at every pass.
pub fn count(columns: &[Pages], passes: usize) -> Result<Vec<[PerByte; 2]>> {
    let counters = pmu::Counters::open()?;
    let max = columns
        .iter()
        .flat_map(|c| c.pages.iter().map(|p| p.1))
        .max()
        .unwrap_or(0);
    let mut out = vec![0; max];
    let mut rows = Vec::new();
    for c in columns {
        let mut readings: [Vec<pmu::Reading>; 2] = [Vec::new(), Vec::new()];
        for pass in 0..passes {
            for side in if pass % 2 == 0 { [0, 1] } else { [1, 0] } {
                let before = counters.read()?;
                decode_all(side, c, &mut out)?;
                readings[side].push(counters.read()?.since(before));
            }
        }
        let bytes = c.output.max(1) as f64;
        rows.push(readings.map(|r| PerByte {
            instructions: median(r.iter().map(|r| r.instructions as f64 / bytes)),
            cycles: median(r.iter().map(|r| r.cycles as f64 / bytes)),
            branches: median(r.iter().map(|r| r.branches as f64 / bytes)),
            misses: median(r.iter().map(|r| r.branch_misses as f64 / bytes)),
        }));
    }
    Ok(rows)
}

fn decode_all(side: usize, c: &Pages, out: &mut [u8]) -> Result<()> {
    let mut reference = snap::raw::Decoder::new();
    for (page, len) in &c.pages {
        let out = &mut out[..*len];
        if side == 0 {
            tessera_parquet::snappy::decompress(page, out)?;
        } else {
            reference.decompress(page, out)?;
        }
        std::hint::black_box(&out);
    }
    Ok(())
}

fn median(values: impl Iterator<Item = f64>) -> f64 {
    let mut v: Vec<f64> = values.collect();
    v.sort_by(f64::total_cmp);
    v.get(v.len() / 2).copied().unwrap_or(f64::NAN)
}

/// The table of the counts: per byte decompressed, the reader's decoder,
/// `snap`'s, and the first over the second.
pub fn table(columns: &[Pages], rows: &[[PerByte; 2]]) -> String {
    let mut s = format!(
        "{:>14} {:>6} {:>7} {:>7} | {:>14} {:>14} {:>14} {:>14} | over snap: {:>6} {:>6} {:>6}\n",
        "column",
        "pages",
        "in MB",
        "out MB",
        "instr/B",
        "cycles/B",
        "branches/B",
        "misses/KB",
        "instr",
        "cycles",
        "misses"
    );
    for (c, [mine, theirs]) in columns.iter().zip(rows) {
        let pair = |a: f64, b: f64| format!("{a:>6.3} {b:>6.3}");
        s += &format!(
            "{:>14} {:>6} {:>7.1} {:>7.1} | {} {} {} {} |            {:>6.3} {:>6.3} {:>6.3}\n",
            c.column,
            c.pages.len(),
            c.input as f64 / 1048576.0,
            c.output as f64 / 1048576.0,
            pair(mine.instructions, theirs.instructions),
            pair(mine.cycles, theirs.cycles),
            pair(mine.branches, theirs.branches),
            pair(mine.misses * 1024.0, theirs.misses * 1024.0),
            mine.instructions / theirs.instructions,
            mine.cycles / theirs.cycles,
            mine.misses / theirs.misses,
        );
    }
    s + "each pair: the reader's decoder, then snap's; medians of the passes\n"
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arrow_array::{ArrayRef, Int64Array, RecordBatch, StringArray};
    use parquet::arrow::ArrowWriter;
    use parquet::basic::Compression;
    use parquet::file::properties::WriterProperties;

    use super::*;

    #[test]
    fn the_pages_of_a_file_are_collected_and_decode_alike() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hits.parquet");
        let urls: Vec<String> = (0..20_000)
            .map(|i| format!("http://example.com/{}", i % 97))
            .collect();
        let batch = RecordBatch::try_from_iter([
            ("URL", Arc::new(StringArray::from(urls)) as ArrayRef),
            (
                "UserID",
                Arc::new(Int64Array::from_iter_values(0..20_000)) as ArrayRef,
            ),
        ])
        .unwrap();
        let props = WriterProperties::builder()
            .set_compression(Compression::SNAPPY)
            .set_max_row_group_row_count(Some(5_000))
            .set_data_page_size_limit(4_096)
            .build();
        let mut writer =
            ArrowWriter::try_new(File::create(&path).unwrap(), batch.schema(), Some(props))
                .unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();

        let columns = collect(&path, &["URL".into(), "UserID".into()], 1 << 30).unwrap();
        assert!(
            columns
                .iter()
                .all(|c| c.pages.len() > 4 && c.output > c.input / 2)
        );
        check(&columns).unwrap();
        assert!(collect(&path, &["Title".into()], 1 << 20).is_err());
    }
}
