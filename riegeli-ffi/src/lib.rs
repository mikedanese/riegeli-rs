#[cxx::bridge(namespace = "riegeli_ffi")]
mod ffi {
    unsafe extern "C++" {
        include!("riegeli-ffi/cpp/wrapper.h");

        type WriterOptions;
        type StringRecordWriter;
        type StringRecordReader;

        // Options
        fn new_writer_options() -> UniquePtr<WriterOptions>;
        fn options_set_transpose(opts: Pin<&mut WriterOptions>, transpose: bool);
        fn options_set_uncompressed(opts: Pin<&mut WriterOptions>);
        fn options_set_brotli(opts: Pin<&mut WriterOptions>, level: i32);
        fn options_set_zstd(opts: Pin<&mut WriterOptions>, level: i32);
        fn options_set_snappy(opts: Pin<&mut WriterOptions>, level: i32);
        fn options_set_window_log(opts: Pin<&mut WriterOptions>, window_log: i32);
        fn options_set_chunk_size(opts: Pin<&mut WriterOptions>, chunk_size: u64);
        fn options_set_bucket_fraction(opts: Pin<&mut WriterOptions>, fraction: f64);
        fn options_set_padding(opts: Pin<&mut WriterOptions>, padding: u64);
        fn options_set_initial_padding(opts: Pin<&mut WriterOptions>, padding: u64);
        fn options_set_final_padding(opts: Pin<&mut WriterOptions>, padding: u64);
        fn options_set_parallelism(opts: Pin<&mut WriterOptions>, parallelism: i32);
        fn options_set_serialized_metadata(opts: Pin<&mut WriterOptions>, data: &[u8]);

        // Writer
        fn new_record_writer(options: UniquePtr<WriterOptions>) -> UniquePtr<StringRecordWriter>;
        fn writer_write_record(writer: Pin<&mut StringRecordWriter>, data: &[u8]) -> bool;
        fn writer_close(writer: Pin<&mut StringRecordWriter>) -> bool;
        fn writer_output_len(writer: &StringRecordWriter) -> usize;
        fn writer_copy_output(writer: &StringRecordWriter, dest: &mut [u8]);
        fn writer_ok(writer: &StringRecordWriter) -> bool;
        fn writer_status_message(writer: &StringRecordWriter) -> String;

        // Reader
        fn new_record_reader(input: &[u8]) -> UniquePtr<StringRecordReader>;
        fn reader_read_next(reader: Pin<&mut StringRecordReader>) -> bool;
        unsafe fn reader_last_record_ptr(reader: &StringRecordReader) -> *const u8;
        fn reader_last_record_len(reader: &StringRecordReader) -> usize;
        fn reader_close(reader: Pin<&mut StringRecordReader>) -> bool;
        fn reader_ok(reader: &StringRecordReader) -> bool;
        fn reader_status_message(reader: &StringRecordReader) -> String;
        fn reader_read_serialized_metadata(
            reader: Pin<&mut StringRecordReader>,
            metadata_out: &mut Vec<u8>,
        ) -> bool;
    }
}

/// Compression algorithm for a C++ RecordWriter.
pub enum Compression {
    None,
    Brotli(i32),
    Zstd(i32),
    Snappy(i32),
}

/// Structured options for creating a C++ RecordWriter.
pub struct WriterOptions {
    inner: cxx::UniquePtr<ffi::WriterOptions>,
}

impl Default for WriterOptions {
    fn default() -> Self {
        Self::new()
    }
}

impl WriterOptions {
    pub fn new() -> Self {
        Self {
            inner: ffi::new_writer_options(),
        }
    }

    pub fn compression(mut self, compression: Compression) -> Self {
        match compression {
            Compression::None => ffi::options_set_uncompressed(self.inner.pin_mut()),
            Compression::Brotli(level) => ffi::options_set_brotli(self.inner.pin_mut(), level),
            Compression::Zstd(level) => ffi::options_set_zstd(self.inner.pin_mut(), level),
            Compression::Snappy(level) => ffi::options_set_snappy(self.inner.pin_mut(), level),
        }
        self
    }

    pub fn transpose(mut self, transpose: bool) -> Self {
        ffi::options_set_transpose(self.inner.pin_mut(), transpose);
        self
    }

    pub fn window_log(mut self, window_log: i32) -> Self {
        ffi::options_set_window_log(self.inner.pin_mut(), window_log);
        self
    }

    pub fn chunk_size(mut self, chunk_size: u64) -> Self {
        ffi::options_set_chunk_size(self.inner.pin_mut(), chunk_size);
        self
    }

    pub fn bucket_fraction(mut self, fraction: f64) -> Self {
        ffi::options_set_bucket_fraction(self.inner.pin_mut(), fraction);
        self
    }

    pub fn padding(mut self, padding: u64) -> Self {
        ffi::options_set_padding(self.inner.pin_mut(), padding);
        self
    }

    pub fn initial_padding(mut self, padding: u64) -> Self {
        ffi::options_set_initial_padding(self.inner.pin_mut(), padding);
        self
    }

    pub fn final_padding(mut self, padding: u64) -> Self {
        ffi::options_set_final_padding(self.inner.pin_mut(), padding);
        self
    }

    pub fn parallelism(mut self, parallelism: i32) -> Self {
        ffi::options_set_parallelism(self.inner.pin_mut(), parallelism);
        self
    }

    pub fn serialized_metadata(mut self, data: &[u8]) -> Self {
        ffi::options_set_serialized_metadata(self.inner.pin_mut(), data);
        self
    }
}

/// A C++ RecordWriter that writes to an in-memory buffer.
pub struct RecordWriter {
    inner: cxx::UniquePtr<ffi::StringRecordWriter>,
}

impl RecordWriter {
    pub fn new(options: WriterOptions) -> Result<Self, String> {
        let inner = ffi::new_record_writer(options.inner);
        if inner.is_null() {
            return Err("failed to create writer".into());
        }
        if !ffi::writer_ok(&inner) {
            return Err(ffi::writer_status_message(&inner));
        }
        Ok(Self { inner })
    }

    pub fn write_record(&mut self, data: &[u8]) -> Result<(), String> {
        if !ffi::writer_write_record(self.inner.pin_mut(), data) {
            return Err(ffi::writer_status_message(&self.inner));
        }
        Ok(())
    }

    /// Close the writer and return the serialized riegeli file bytes.
    pub fn close(mut self) -> Result<Vec<u8>, String> {
        if !ffi::writer_close(self.inner.pin_mut()) {
            return Err(ffi::writer_status_message(&self.inner));
        }
        let len = ffi::writer_output_len(&self.inner);
        let mut buf = vec![0u8; len];
        ffi::writer_copy_output(&self.inner, &mut buf);
        Ok(buf)
    }
}

/// A C++ RecordReader that reads from an in-memory buffer.
pub struct RecordReader {
    inner: cxx::UniquePtr<ffi::StringRecordReader>,
}

impl RecordReader {
    /// Create a reader from riegeli file bytes.
    pub fn new(data: &[u8]) -> Result<Self, String> {
        let inner = ffi::new_record_reader(data);
        if inner.is_null() {
            return Err("failed to create reader".into());
        }
        if !ffi::reader_ok(&inner) {
            return Err(ffi::reader_status_message(&inner));
        }
        Ok(Self { inner })
    }

    /// Advance to the next record. Returns `false` at end of file.
    /// After a successful call, use [`last_record`](Self::last_record) to
    /// borrow the record data without copying. The borrow is valid until the
    /// next call to `read_next` or `close`.
    pub fn read_next(&mut self) -> Result<bool, String> {
        if ffi::reader_read_next(self.inner.pin_mut()) {
            Ok(true)
        } else if ffi::reader_ok(&self.inner) {
            Ok(false)
        } else {
            Err(ffi::reader_status_message(&self.inner))
        }
    }

    /// Borrow the record returned by the last successful [`read_next`](Self::read_next) call.
    /// The slice borrows directly from the C++ reader's internal buffers (zero-copy).
    pub fn last_record(&self) -> &[u8] {
        let ptr = unsafe { ffi::reader_last_record_ptr(&self.inner) };
        let len = ffi::reader_last_record_len(&self.inner);
        if ptr.is_null() || len == 0 {
            return &[];
        }
        unsafe { std::slice::from_raw_parts(ptr, len) }
    }

    /// Read the next record, returning an owned copy. Returns `None` at end of file.
    pub fn read_record(&mut self) -> Result<Option<Vec<u8>>, String> {
        if self.read_next()? {
            Ok(Some(self.last_record().to_vec()))
        } else {
            Ok(None)
        }
    }

    /// Read the file metadata as raw serialized bytes, if present.
    /// Returns `None` if no metadata was written or if metadata is empty.
    pub fn read_serialized_metadata(&mut self) -> Result<Option<Vec<u8>>, String> {
        let mut metadata = Vec::new();
        if ffi::reader_read_serialized_metadata(self.inner.pin_mut(), &mut metadata) {
            if metadata.is_empty() {
                Ok(None)
            } else {
                Ok(Some(metadata))
            }
        } else if ffi::reader_ok(&self.inner) {
            Ok(None)
        } else {
            Err(ffi::reader_status_message(&self.inner))
        }
    }

    pub fn close(mut self) -> Result<(), String> {
        if !ffi::reader_close(self.inner.pin_mut()) {
            return Err(ffi::reader_status_message(&self.inner));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_defaults() {
        let mut writer = RecordWriter::new(WriterOptions::new()).unwrap();
        writer.write_record(b"hello").unwrap();
        writer.write_record(b"world").unwrap();
        let data = writer.close().unwrap();

        let mut reader = RecordReader::new(&data).unwrap();
        assert_eq!(reader.read_record().unwrap().unwrap(), b"hello");
        assert_eq!(reader.read_record().unwrap().unwrap(), b"world");
        assert!(reader.read_record().unwrap().is_none());
        reader.close().unwrap();
    }

    #[test]
    fn roundtrip_all_compression_modes() {
        let configs: Vec<(&str, WriterOptions)> = vec![
            (
                "uncompressed",
                WriterOptions::new().compression(Compression::None),
            ),
            (
                "brotli:6",
                WriterOptions::new().compression(Compression::Brotli(6)),
            ),
            (
                "zstd:3",
                WriterOptions::new().compression(Compression::Zstd(3)),
            ),
            (
                "snappy:1",
                WriterOptions::new().compression(Compression::Snappy(1)),
            ),
            (
                "transpose+brotli",
                WriterOptions::new()
                    .transpose(true)
                    .compression(Compression::Brotli(6)),
            ),
            (
                "transpose+zstd",
                WriterOptions::new()
                    .transpose(true)
                    .compression(Compression::Zstd(3)),
            ),
        ];

        for (label, opts) in configs {
            let mut writer = RecordWriter::new(opts).unwrap();
            for i in 0..100 {
                writer
                    .write_record(format!("record_{i:06}").as_bytes())
                    .unwrap();
            }
            let data = writer.close().unwrap();
            assert!(!data.is_empty(), "empty output for {label}");

            let mut reader = RecordReader::new(&data).unwrap();
            for i in 0..100 {
                let rec = reader.read_record().unwrap().unwrap();
                assert_eq!(
                    rec,
                    format!("record_{i:06}").as_bytes(),
                    "mismatch in {label}"
                );
            }
            assert!(reader.read_record().unwrap().is_none());
            reader.close().unwrap();
        }
    }

    #[test]
    fn writer_options_builder() {
        let opts = WriterOptions::new()
            .transpose(true)
            .compression(Compression::Zstd(5))
            .chunk_size(1 << 20)
            .bucket_fraction(0.5)
            .initial_padding(4096)
            .final_padding(4096);

        let mut writer = RecordWriter::new(opts).unwrap();
        writer.write_record(b"test").unwrap();
        let data = writer.close().unwrap();
        assert!(!data.is_empty());
    }

    #[test]
    fn metadata_round_trip() {
        let metadata = b"some serialized metadata bytes";
        let opts = WriterOptions::new().serialized_metadata(metadata);
        let mut writer = RecordWriter::new(opts).unwrap();
        writer.write_record(b"record").unwrap();
        let data = writer.close().unwrap();

        let mut reader = RecordReader::new(&data).unwrap();
        let got = reader.read_serialized_metadata().unwrap();
        assert_eq!(got.as_deref(), Some(metadata.as_slice()));
        // Records still readable after metadata read
        assert_eq!(reader.read_record().unwrap().unwrap(), b"record");
        reader.close().unwrap();
    }

    #[test]
    fn metadata_absent() {
        let mut writer = RecordWriter::new(WriterOptions::new()).unwrap();
        writer.write_record(b"record").unwrap();
        let data = writer.close().unwrap();

        let mut reader = RecordReader::new(&data).unwrap();
        let got = reader.read_serialized_metadata().unwrap();
        assert!(got.is_none());
        reader.close().unwrap();
    }
}
