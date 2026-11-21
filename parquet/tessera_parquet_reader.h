/*
 * The entry points of the reader of Parquet files, the Rust crate
 * crates/tessera-parquet, which the module tessera_parquet links as a
 * static library.
 *
 * Every entry point that can fail returns true on success and false with
 * its status filled; the caller raises the error after the call has
 * returned, so that no Rust frame is on the stack when PostgreSQL's error
 * unwinds. A panic inside the reader is caught and comes back as a
 * status: XX001 while a file is decoded, XX000 otherwise. The reader
 * calls no function of PostgreSQL. Its objects are the reader's own,
 * freed by the entry points of their type; memory the caller gives it,
 * such as an arena of text, it only fills. Masks are arrays of 64-bit
 * words, bit i % 64 of word i / 64 for row i.
 */
#ifndef TESSERA_PARQUET_READER_H
#define TESSERA_PARQUET_READER_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#define TESS_PARQUET_MESSAGE_SIZE 256

/* The status of a call that failed. */
typedef struct TessParquetStatus
{
	char		sqlstate[6];	/* five characters and a terminator */
	int64_t		row;			/* the row of the value that failed, or -1 */
	char		message[TESS_PARQUET_MESSAGE_SIZE];
} TessParquetStatus;

/* What fstat said of a file when it was opened. */
typedef struct TessParquetIdentity
{
	uint64_t	size;
	int64_t		mtime_sec;
	int64_t		mtime_nsec;
	uint64_t	dev;
	uint64_t	ino;
} TessParquetIdentity;

typedef struct TessParquetFile TessParquetFile;
typedef struct TessParquetColumn TessParquetColumn;
typedef struct TessParquetReader TessParquetReader;

/* The values of the option unit, and of the option utf8. */
#define TESS_PARQUET_UNIT_NONE			(-1)
#define TESS_PARQUET_UNIT_DAYS			0
#define TESS_PARQUET_UNIT_SECONDS		1
#define TESS_PARQUET_UNIT_MILLISECONDS	2
#define TESS_PARQUET_UNIT_MICROSECONDS	3
#define TESS_PARQUET_UTF8_NONE			(-1)
#define TESS_PARQUET_UTF8_CHECK			0
#define TESS_PARQUET_UTF8_TRUST			1

/*
 * Opens the file at the path of path_len bytes and reads its footer;
 * where expected is not NULL, refuses a file of another identity, 55000.
 * 58P01 for a file that does not exist, 58030 for one that cannot be read,
 * XX001 for a footer the format cannot decode.
 */
extern bool tess_parquet_open(const char *path, size_t path_len,
							  const TessParquetIdentity *expected,
							  TessParquetFile **out,
							  TessParquetStatus *status);
/* Frees a file; its descriptor is closed once no reader of it is left. */
extern void tess_parquet_file_free(TessParquetFile *file);
extern bool tess_parquet_file_info(const TessParquetFile *file,
								   TessParquetIdentity *identity,
								   size_t *groups, uint64_t *rows,
								   TessParquetStatus *status);
extern bool tess_parquet_group_rows(const TessParquetFile *file, size_t group,
									uint64_t *rows,
									TessParquetStatus *status);
/* 55000 where the open file's size or time changed. */
extern bool tess_parquet_check(const TessParquetFile *file,
							   TessParquetStatus *status);

/*
 * Maps a column of a foreign table to the file column named name, of the
 * declared type type_oid and typmod, with the options unit and utf8 and
 * the database's encoding: 42703, 0A000, 22023, 42804, 54000 in the
 * order of the spec's requirement "The columns and their types".
 */
extern bool tess_parquet_column(const TessParquetFile *file,
								const char *name, size_t name_len,
								uint32_t type_oid, int32_t typmod,
								int32_t unit, int32_t utf8,
								bool server_utf8,
								TessParquetColumn **out,
								TessParquetStatus *status);
extern void tess_parquet_column_free(TessParquetColumn *column);

/*
 * A reader of a column of one group of rows, for the rows of a selection
 * of selection_words words over the group; its arrays hold batch
 * selected rows.
 */
extern bool tess_parquet_reader(const TessParquetFile *file,
								const TessParquetColumn *column,
								size_t group, const uint64_t *selection,
								size_t selection_words, size_t batch,
								TessParquetReader **out,
								TessParquetStatus *status);
extern void tess_parquet_reader_free(TessParquetReader *reader);

/*
 * Reads the window of len rows from the group's row start. Where it
 * starts where the last forward window ended, or is the first, every
 * selected row of it is read; otherwise the rows the mask rows asks for.
 * Writes the mask of the rows the window holds, over the window, and
 * their count; the window is the reader's until the next read.
 */
extern bool tess_parquet_read(TessParquetReader *reader, size_t start,
							  size_t len, const uint64_t *rows,
							  size_t rows_words, uint64_t *held,
							  size_t held_words, size_t *count,
							  TessParquetStatus *status);

/*
 * Converts the rows the mask asked asks of the window's rows held, in
 * their order, into Datum words and a mask of NULL rows, for a type
 * passed by value; values holds at least the window's count. For a plain
 * column of integers, floating-point numbers, dates, times or booleans,
 * the place of a row not asked may be written with its true value.
 */
extern bool tess_parquet_values(TessParquetReader *reader,
								const uint64_t *asked, size_t asked_words,
								uint64_t *values, size_t values_len,
								uint64_t *nulls, size_t nulls_words,
								TessParquetStatus *status);

/*
 * Checks the text of the rows asked asks of the window (22021, 22001,
 * 54000 at the first row that fails), writes their mask of NULL rows and
 * the bytes of the arena their values need; then tess_parquet_text
 * writes each value with its header into the arena, aligned to 4 bytes,
 * and its Datum, the value's address, into values, leaving the places of
 * the other rows as they were.
 */
extern bool tess_parquet_text_size(TessParquetReader *reader,
								   const uint64_t *asked, size_t asked_words,
								   uint64_t *nulls, size_t nulls_words,
								   size_t *bytes, TessParquetStatus *status);
extern bool tess_parquet_text(TessParquetReader *reader, uint8_t *arena,
							  size_t arena_len, uint64_t *values,
							  size_t values_len, TessParquetStatus *status);

/* Raises a panic inside the reader, for the test of the boundary. */
extern bool tess_parquet_test_panic(TessParquetStatus *status);

#endif							/* TESSERA_PARQUET_READER_H */
