#pragma once

#include <cstdint>
#include <memory>
#include <string>

#include "riegeli/bytes/string_reader.h"
#include "riegeli/bytes/string_writer.h"
#include "riegeli/records/record_reader.h"
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
