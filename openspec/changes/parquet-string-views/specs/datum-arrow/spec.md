## ADDED Requirements

### Requirement: Views of strings
The crate SHALL read the Arrow view arrays `BinaryView` and `Utf8View`
as it reads `LargeBinary` and `LargeUtf8`: a read plan SHALL accept the
same pairs, name the same failures and give the same representations. A
dictionary of views SHALL be refused, `0A000`. A view is a record of 16
bytes: a value's length and either the value itself, of up to 12 bytes,
or the place of its bytes in one of the array's buffers. The layout of a
view and the input of a view array, its views and its buffers, are
written in [`Values`](../../../crates/datum-arrow/src/column.rs); its
broken shapes in
[`ReadPlan::read`](../../../crates/datum-arrow/src/read.rs).

#### Scenario: Pairs of views
- **WHEN** read plans are made for `BinaryView` into `bytea`, checked
  into `text`, `varchar` and `varchar(10)`, and trusted into `text` and
  `varchar`; for `Utf8View` into `text`, `varchar` and `varchar(10)`;
  and for a `Dictionary` of `Utf8View` and one of `BinaryView`
- **THEN** each but the dictionaries is accepted and names the failures
  of its twin of `LargeBinary` or `LargeUtf8`, and the dictionaries are
  refused with `0A000`
- **Verified by:**
  `crates/datum-arrow/src/read/tests.rs::pairs_of_views`;
  `crates/datum-arrow/src/arrow.rs::every_plain_type_taken`;
  `crates/datum-arrow/src/arrow.rs::types_outside_the_set`

#### Scenario: Views read as their twins
- **WHEN** columns of views are read: values of 0, 1, 12 and 13 bytes
  and longer, inside their views and in two buffers; strings that are
  not UTF-8, hold a zero byte or are too long for a `varchar(3)`;
  `NULL` slots whose views point past every buffer; and slots not asked
  for
- **THEN** each gives the representations and the failure, at the same
  slot, that the same values give from `LargeBinary` and `LargeUtf8`
  under the same plan, and the `NULL` slots and the slots not asked for
  fail in nothing
- **Verified by:**
  `crates/datum-arrow/src/read/tests.rs::views_read_as_their_twins`;
  `crates/datum-arrow/src/read/tests.rs::views_of_arrow_s_own_array`;
  `crates/datum-arrow/src/read/tests.rs::no_panic_on_any_input`

#### Scenario: Views of a broken shape
- **WHEN** a read is called over views fewer than the column's slots,
  and over a view asked for whose buffer is past the buffers given or
  whose bytes run past the end of its buffer
- **THEN** each call gives `XX000`, its output unchanged, and none
  panics
- **Verified by:**
  `crates/datum-arrow/src/read/tests.rs::views_of_a_broken_shape`
