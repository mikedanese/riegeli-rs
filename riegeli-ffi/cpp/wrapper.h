#pragma once

#include <cstdint>
#include <memory>
#include <string>
#include <vector>

#include "riegeli/bytes/string_reader.h"
#include "riegeli/bytes/string_writer.h"
#include "riegeli/chunk_encoding/field_projection.h"
#include "riegeli/records/record_reader.h"
#include "riegeli/records/skipped_region.h"
#include "riegeli/records/record_writer.h"
#include "rust/cxx.h"

namespace riegeli_ffi {

// Opaque options builder, manipulated via free functions.
struct WriterOptions {
  riegeli::RecordWriterBase::Options inner;
};

struct StringRecordWriter {
  std::string output;
  std::unique_ptr<riegeli::RecordWriter<riegeli::StringWriter<std::string*>>>
      writer;
  std::string error_message;
  bool is_ok = true;
};

struct StringRecordReader {
  std::string input;
  std::unique_ptr<riegeli::RecordReader<riegeli::StringReader<>>> reader;
  const uint8_t* last_record_ptr = nullptr;
  size_t last_record_size = 0;
  std::string error_message;
  bool is_ok = true;
  // Populated when the reader was created with collecting recovery.
  std::shared_ptr<std::vector<riegeli::SkippedRegion>> skipped;
};

// Options
std::unique_ptr<WriterOptions> new_writer_options();
void options_set_transpose(WriterOptions& opts, bool transpose);
void options_set_uncompressed(WriterOptions& opts);
void options_set_brotli(WriterOptions& opts, int level);
void options_set_zstd(WriterOptions& opts, int level);
void options_set_snappy(WriterOptions& opts, int level);
void options_set_window_log(WriterOptions& opts, int window_log);
void options_set_chunk_size(WriterOptions& opts, uint64_t chunk_size);
void options_set_bucket_fraction(WriterOptions& opts, double fraction);
void options_set_padding(WriterOptions& opts, uint64_t padding);
void options_set_initial_padding(WriterOptions& opts, uint64_t padding);
void options_set_final_padding(WriterOptions& opts, uint64_t padding);
void options_set_parallelism(WriterOptions& opts, int parallelism);

// Writer options: metadata
void options_set_serialized_metadata(WriterOptions& opts,
                                     rust::Slice<const uint8_t> data);

// Writer
std::unique_ptr<StringRecordWriter> new_record_writer(
    std::unique_ptr<WriterOptions> options);
bool writer_write_record(StringRecordWriter& writer,
                         rust::Slice<const uint8_t> data);
bool writer_close(StringRecordWriter& writer);
size_t writer_output_len(const StringRecordWriter& writer);
void writer_copy_output(const StringRecordWriter& writer,
                        rust::Slice<uint8_t> dest);
bool writer_ok(const StringRecordWriter& writer);
rust::String writer_status_message(const StringRecordWriter& writer);

// Reader
std::unique_ptr<StringRecordReader> new_record_reader(
    rust::Slice<const uint8_t> input);

// Reader with options, for differential testing against the Rust
// implementation:
// - `projection_paths_flat`: field-number paths separated by the sentinel
//   0xFFFFFFFF; an existence-only terminus is expressed the C++ way, as a
//   trailing 0 (Field::kExistenceOnly). Empty slice = no projection.
// - `collect_recovery`: install a recovery callback that records every
//   SkippedRegion (exposed via the reader_skipped_* accessors below).
// - `cancel_after`: with collecting recovery, return false (cancel) from the
//   callback after this many regions have been collected; -1 = never cancel.
std::unique_ptr<StringRecordReader> new_record_reader_with_options(
    rust::Slice<const uint8_t> input,
    rust::Slice<const uint32_t> projection_paths_flat, bool collect_recovery,
    int32_t cancel_after);

// Collected SkippedRegions (empty unless created with collect_recovery).
size_t reader_skipped_count(const StringRecordReader& reader);
uint64_t reader_skipped_begin(const StringRecordReader& reader, size_t i);
uint64_t reader_skipped_end(const StringRecordReader& reader, size_t i);
rust::String reader_skipped_message(const StringRecordReader& reader,
                                    size_t i);

// Current position (numeric form), for the region/resync coupling checks.
uint64_t reader_pos_numeric(const StringRecordReader& reader);

// Seek to a numeric position (C++ Seek(Position)); returns the C++ result.
bool reader_seek_numeric(StringRecordReader& reader, uint64_t pos);
bool reader_read_next(StringRecordReader& reader);
const uint8_t* reader_last_record_ptr(const StringRecordReader& reader);
size_t reader_last_record_len(const StringRecordReader& reader);
bool reader_close(StringRecordReader& reader);
bool reader_ok(const StringRecordReader& reader);
rust::String reader_status_message(const StringRecordReader& reader);

// Reader: metadata
bool reader_read_serialized_metadata(StringRecordReader& reader,
                                     rust::Vec<uint8_t>& metadata_out);

}  // namespace riegeli_ffi
