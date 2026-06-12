#include <algorithm>
#include <cstring>
#include <string>
#include <utility>

#include "riegeli-ffi/cpp/wrapper.h"

namespace riegeli_ffi {

// --- Options ---

std::unique_ptr<WriterOptions> new_writer_options() {
  return std::make_unique<WriterOptions>();
}

void options_set_transpose(WriterOptions& opts, bool transpose) {
  opts.inner.set_transpose(transpose);
}

void options_set_uncompressed(WriterOptions& opts) {
  opts.inner.set_uncompressed();
}

void options_set_brotli(WriterOptions& opts, int level) {
  opts.inner.set_brotli(level);
}

void options_set_zstd(WriterOptions& opts, int level) {
  opts.inner.set_zstd(level);
}

void options_set_snappy(WriterOptions& opts, int level) {
  opts.inner.set_snappy(level);
}

void options_set_window_log(WriterOptions& opts, int window_log) {
  opts.inner.set_window_log(window_log);
}

void options_set_chunk_size(WriterOptions& opts, uint64_t chunk_size) {
  opts.inner.set_chunk_size(chunk_size);
}

void options_set_bucket_fraction(WriterOptions& opts, double fraction) {
  opts.inner.set_bucket_fraction(fraction);
}

void options_set_padding(WriterOptions& opts, uint64_t padding) {
  opts.inner.set_padding(padding);
}

void options_set_initial_padding(WriterOptions& opts, uint64_t padding) {
  opts.inner.set_initial_padding(padding);
}

void options_set_final_padding(WriterOptions& opts, uint64_t padding) {
  opts.inner.set_final_padding(padding);
}

void options_set_parallelism(WriterOptions& opts, int parallelism) {
  opts.inner.set_parallelism(parallelism);
}

void options_set_serialized_metadata(WriterOptions& opts,
                                     rust::Slice<const uint8_t> data) {
  riegeli::Chain chain(absl::string_view(
      reinterpret_cast<const char*>(data.data()), data.size()));
  opts.inner.set_serialized_metadata(std::move(chain));
}

// --- Writer ---

std::unique_ptr<StringRecordWriter> new_record_writer(
    std::unique_ptr<WriterOptions> options) {
  auto w = std::make_unique<StringRecordWriter>();
  riegeli::RecordWriterBase::Options opts;
  if (options) {
    opts = std::move(options->inner);
  }
  w->writer = std::make_unique<
      riegeli::RecordWriter<riegeli::StringWriter<std::string*>>>(
      riegeli::Maker(&w->output), std::move(opts));
  if (!w->writer->ok()) {
    w->is_ok = false;
    w->error_message = w->writer->status().ToString();
  }
  return w;
}

bool writer_write_record(StringRecordWriter& writer,
                         rust::Slice<const uint8_t> data) {
  if (!writer.writer || !writer.is_ok) return false;
  absl::string_view record(reinterpret_cast<const char*>(data.data()),
                           data.size());
  if (!writer.writer->WriteRecord(record)) {
    writer.is_ok = false;
    writer.error_message = writer.writer->status().ToString();
    return false;
  }
  return true;
}

bool writer_close(StringRecordWriter& writer) {
  if (!writer.writer) return false;
  if (!writer.writer->Close()) {
    writer.is_ok = false;
    writer.error_message = writer.writer->status().ToString();
    return false;
  }
  return true;
}

size_t writer_output_len(const StringRecordWriter& writer) {
  return writer.output.size();
}

void writer_copy_output(const StringRecordWriter& writer,
                        rust::Slice<uint8_t> dest) {
  const size_t n = std::min(dest.size(), writer.output.size());
  if (n > 0) std::memcpy(dest.data(), writer.output.data(), n);
}

bool writer_ok(const StringRecordWriter& writer) { return writer.is_ok; }

rust::String writer_status_message(const StringRecordWriter& writer) {
  return rust::String(writer.error_message);
}

// --- Reader ---

std::unique_ptr<StringRecordReader> new_record_reader(
    rust::Slice<const uint8_t> input) {
  auto r = std::make_unique<StringRecordReader>();
  r->input.assign(reinterpret_cast<const char*>(input.data()), input.size());
  r->reader = std::make_unique<riegeli::RecordReader<riegeli::StringReader<>>>(
      riegeli::Maker(absl::string_view(r->input)));
  if (!r->reader->ok()) {
    r->is_ok = false;
    r->error_message = r->reader->status().ToString();
  }
  return r;
}

std::unique_ptr<StringRecordReader> new_record_reader_with_options(
    rust::Slice<const uint8_t> input,
    rust::Slice<const uint32_t> projection_paths_flat, bool collect_recovery,
    int32_t cancel_after) {
  auto r = std::make_unique<StringRecordReader>();
  r->input.assign(reinterpret_cast<const char*>(input.data()), input.size());

  riegeli::RecordReaderBase::Options opts;

  if (!projection_paths_flat.empty()) {
    riegeli::FieldProjection projection;
    riegeli::Field field;
    for (size_t i = 0; i < projection_paths_flat.size(); ++i) {
      const uint32_t v = projection_paths_flat[i];
      if (v == 0xFFFFFFFFu) {
        projection.AddField(std::move(field));
        field = riegeli::Field();
      } else {
        field.AddFieldNumber(static_cast<int>(v));
      }
    }
    projection.AddField(std::move(field));
    opts.set_field_projection(std::move(projection));
  }

  if (collect_recovery) {
    r->skipped = std::make_shared<std::vector<riegeli::SkippedRegion>>();
    auto skipped = r->skipped;
    const int32_t cancel_at = cancel_after;
    opts.set_recovery(
        [skipped, cancel_at](const riegeli::SkippedRegion& region,
                             riegeli::RecordReaderBase&) {
          skipped->push_back(region);
          if (cancel_at >= 0 &&
              skipped->size() > static_cast<size_t>(cancel_at)) {
            return false;
          }
          return true;
        });
  }

  r->reader = std::make_unique<riegeli::RecordReader<riegeli::StringReader<>>>(
      riegeli::Maker(absl::string_view(r->input)), std::move(opts));
  if (!r->reader->ok()) {
    r->is_ok = false;
    r->error_message = r->reader->status().ToString();
  }
  return r;
}

size_t reader_skipped_count(const StringRecordReader& reader) {
  return reader.skipped ? reader.skipped->size() : 0;
}

uint64_t reader_skipped_begin(const StringRecordReader& reader, size_t i) {
  if (!reader.skipped || i >= reader.skipped->size()) return 0;
  return (*reader.skipped)[i].begin();
}

uint64_t reader_skipped_end(const StringRecordReader& reader, size_t i) {
  if (!reader.skipped || i >= reader.skipped->size()) return 0;
  return (*reader.skipped)[i].end();
}

rust::String reader_skipped_message(const StringRecordReader& reader,
                                    size_t i) {
  if (!reader.skipped || i >= reader.skipped->size()) return rust::String();
  return rust::String(std::string((*reader.skipped)[i].message()));
}

uint64_t reader_pos_numeric(const StringRecordReader& reader) {
  if (!reader.reader) return 0;
  return reader.reader->pos().numeric();
}

// The string_view returned by ReadRecord borrows the reader's internal
// buffers and is invalidated by the next non-const operation on the reader.
// Every entry point that mutates the reader must drop the cached view so
// that last_record cannot hand out a dangling slice.
static void clear_last_record(StringRecordReader& reader) {
  reader.last_record_ptr = nullptr;
  reader.last_record_size = 0;
}

bool reader_seek_numeric(StringRecordReader& reader, uint64_t pos) {
  if (!reader.reader) return false;
  clear_last_record(reader);
  const bool ok = reader.reader->Seek(riegeli::Position{pos});
  if (!ok && !reader.reader->ok()) {
    reader.is_ok = false;
    reader.error_message = reader.reader->status().ToString();
  }
  return ok;
}

bool reader_read_next(StringRecordReader& reader) {
  if (!reader.reader || !reader.is_ok) return false;
  absl::string_view record;
  if (!reader.reader->ReadRecord(record)) {
    if (!reader.reader->ok()) {
      reader.is_ok = false;
      reader.error_message = reader.reader->status().ToString();
    }
    clear_last_record(reader);
    return false;
  }
  reader.last_record_ptr = reinterpret_cast<const uint8_t*>(record.data());
  reader.last_record_size = record.size();
  return true;
}

const uint8_t* reader_last_record_ptr(const StringRecordReader& reader) {
  return reader.last_record_ptr;
}

size_t reader_last_record_len(const StringRecordReader& reader) {
  return reader.last_record_size;
}

bool reader_close(StringRecordReader& reader) {
  if (!reader.reader) return false;
  clear_last_record(reader);
  if (!reader.reader->Close()) {
    reader.is_ok = false;
    reader.error_message = reader.reader->status().ToString();
    return false;
  }
  return true;
}

bool reader_ok(const StringRecordReader& reader) { return reader.is_ok; }

rust::String reader_status_message(const StringRecordReader& reader) {
  return rust::String(reader.error_message);
}

bool reader_read_serialized_metadata(StringRecordReader& reader,
                                     rust::Vec<uint8_t>& metadata_out) {
  if (!reader.reader || !reader.is_ok) return false;
  clear_last_record(reader);
  riegeli::Chain metadata;
  if (!reader.reader->ReadSerializedMetadata(metadata)) {
    if (!reader.reader->ok()) {
      reader.is_ok = false;
      reader.error_message = reader.reader->status().ToString();
    }
    return false;
  }
  metadata_out.clear();
  metadata_out.reserve(metadata.size());
  for (absl::string_view fragment : metadata.blocks()) {
    for (char c : fragment) {
      metadata_out.push_back(static_cast<uint8_t>(c));
    }
  }
  return true;
}

}  // namespace riegeli_ffi
