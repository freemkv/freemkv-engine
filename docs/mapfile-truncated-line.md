# `load_rejects_a_data_line_with_too_few_fields` rationale

A truncated data line is REFUSED, not skipped.

`fields.len() < 3` used to `continue`, which deletes that line's range
from a partition every consumer reads as gapless — and when the short
line is the LAST one it also shrinks `total_size`, since that is
derived from the final entry's end. In this test the truncated line is
the tail of the disc: skipping it reports a 0x2800-byte disc that is
100% good, erasing 0x800 bytes of coverage from every consumer.

The expectation is a literal, not a value re-derived from the parser:
the skip answer is `total_size == 0x2800`, and that is what must not
happen.
