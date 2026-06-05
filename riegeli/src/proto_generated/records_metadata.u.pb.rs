const _: () = ::protobuf::__internal::assert_compatible_gencode_version("4.34.1-release");
// This variable must not be referenced except by protobuf generated
// code.
pub(crate) static mut riegeli__RecordsMetadata_msg_init: ::protobuf::__internal::runtime::MiniTableInitPtr =
    ::protobuf::__internal::runtime::MiniTableInitPtr(::protobuf::__internal::runtime::MiniTablePtr::dangling());
#[allow(non_camel_case_types)]
pub struct RecordsMetadata {
  inner: ::protobuf::__internal::runtime::OwnedMessageInner<RecordsMetadata>
}

impl ::protobuf::Message for RecordsMetadata {}

impl ::std::default::Default for RecordsMetadata {
  fn default() -> Self {
    Self::new()
  }
}

impl ::std::fmt::Debug for RecordsMetadata {
  fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
    write!(f, "{}", ::protobuf::__internal::runtime::debug_string(self))
  }
}

// SAFETY:
// - `RecordsMetadata` is `Sync` because it does not implement interior mutability.
//    Neither does `RecordsMetadataMut`.
unsafe impl Sync for RecordsMetadata {}

// SAFETY:
// - `RecordsMetadata` is `Send` because it uniquely owns its arena and does
//   not use thread-local data.
unsafe impl Send for RecordsMetadata {}

impl ::protobuf::Proxied for RecordsMetadata {
  type View<'msg> = RecordsMetadataView<'msg>;
}

impl ::protobuf::__internal::SealedInternal for RecordsMetadata {}

impl ::protobuf::MutProxied for RecordsMetadata {
  type Mut<'msg> = RecordsMetadataMut<'msg>;
}

#[derive(Copy, Clone)]
#[allow(dead_code)]
pub struct RecordsMetadataView<'msg> {
  inner: ::protobuf::__internal::runtime::MessageViewInner<'msg, RecordsMetadata>,
}

impl<'msg> ::protobuf::__internal::SealedInternal for RecordsMetadataView<'msg> {}

impl<'msg> ::protobuf::MessageView<'msg> for RecordsMetadataView<'msg> {
  type Message = RecordsMetadata;
}

impl ::std::fmt::Debug for RecordsMetadataView<'_> {
  fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
    write!(f, "{}", ::protobuf::__internal::runtime::debug_string(self))
  }
}

impl ::std::default::Default for RecordsMetadataView<'_> {
  fn default() -> RecordsMetadataView<'static> {
    ::protobuf::__internal::runtime::MessageViewInner::default().into()
  }
}

impl<'msg> From<::protobuf::__internal::runtime::MessageViewInner<'msg, RecordsMetadata>> for RecordsMetadataView<'msg> {
  fn from(inner: ::protobuf::__internal::runtime::MessageViewInner<'msg, RecordsMetadata>) -> Self {
    Self { inner }
  }
}

#[allow(dead_code)]
impl<'msg> RecordsMetadataView<'msg> {

  pub fn to_owned(&self) -> RecordsMetadata {
    ::protobuf::IntoProxied::into_proxied(*self, ::protobuf::__internal::Private)
  }

  // file_comment: optional string
  pub fn has_file_comment(self) -> bool {
    unsafe {
      self.inner.ptr().has_field_at_index(0)
    }
  }
  pub fn file_comment_opt(self) -> ::protobuf::Optional<&'msg ::protobuf::ProtoStr> {
        ::protobuf::Optional::new(self.file_comment(), self.has_file_comment())
  }
  pub fn file_comment(self) -> ::protobuf::View<'msg, ::protobuf::ProtoString> {
    let str_view = unsafe {
      self.inner.ptr().get_string_at_index(
        0, (b"").into()
      )
    };
    // SAFETY: The runtime doesn't require ProtoStr to be UTF-8.
    unsafe { ::protobuf::ProtoStr::from_utf8_unchecked(str_view.as_ref()) }
  }

  // record_type_name: optional string
  pub fn has_record_type_name(self) -> bool {
    unsafe {
      self.inner.ptr().has_field_at_index(1)
    }
  }
  pub fn record_type_name_opt(self) -> ::protobuf::Optional<&'msg ::protobuf::ProtoStr> {
        ::protobuf::Optional::new(self.record_type_name(), self.has_record_type_name())
  }
  pub fn record_type_name(self) -> ::protobuf::View<'msg, ::protobuf::ProtoString> {
    let str_view = unsafe {
      self.inner.ptr().get_string_at_index(
        1, (b"").into()
      )
    };
    // SAFETY: The runtime doesn't require ProtoStr to be UTF-8.
    unsafe { ::protobuf::ProtoStr::from_utf8_unchecked(str_view.as_ref()) }
  }

  // file_descriptor: repeated message google.protobuf.FileDescriptorProto
  pub fn file_descriptor(self) -> ::protobuf::RepeatedView<'msg, super::FileDescriptorProto> {
    unsafe {
      self.inner.ptr().get_array_at_index(
        2
      )
    }.map_or_else(
        ::protobuf::__internal::runtime::empty_array::<super::FileDescriptorProto>,
        |raw| unsafe {
          ::protobuf::RepeatedView::from_raw(::protobuf::__internal::Private, raw)
        }
      )
  }

  // record_writer_options: optional string
  pub fn has_record_writer_options(self) -> bool {
    unsafe {
      self.inner.ptr().has_field_at_index(3)
    }
  }
  pub fn record_writer_options_opt(self) -> ::protobuf::Optional<&'msg ::protobuf::ProtoStr> {
        ::protobuf::Optional::new(self.record_writer_options(), self.has_record_writer_options())
  }
  pub fn record_writer_options(self) -> ::protobuf::View<'msg, ::protobuf::ProtoString> {
    let str_view = unsafe {
      self.inner.ptr().get_string_at_index(
        3, (b"").into()
      )
    };
    // SAFETY: The runtime doesn't require ProtoStr to be UTF-8.
    unsafe { ::protobuf::ProtoStr::from_utf8_unchecked(str_view.as_ref()) }
  }

  // num_records: optional int64
  pub fn has_num_records(self) -> bool {
    unsafe {
      self.inner.ptr().has_field_at_index(4)
    }
  }
  pub fn num_records_opt(self) -> ::protobuf::Optional<i64> {
        ::protobuf::Optional::new(self.num_records(), self.has_num_records())
  }
  pub fn num_records(self) -> i64 {
    unsafe {
      // TODO: b/361751487: This .into() and .try_into() is only
      // here for the enum<->i32 case, we should avoid it for
      // other primitives where the types naturally match
      // perfectly (and do an unchecked conversion for
      // i32->enum types, since even for closed enums we trust
      // upb to only return one of the named values).
      self.inner.ptr().get_i64_at_index(
        4, (0i64).into()
      ).try_into().unwrap()
    }
  }

}

// SAFETY:
// - `RecordsMetadataView` is `Sync` because it does not support mutation.
unsafe impl Sync for RecordsMetadataView<'_> {}

// SAFETY:
// - `RecordsMetadataView` is `Send` because while its alive a `RecordsMetadataMut` cannot.
// - `RecordsMetadataView` does not use thread-local data.
unsafe impl Send for RecordsMetadataView<'_> {}

impl<'msg> ::protobuf::AsView for RecordsMetadataView<'msg> {
  type Proxied = RecordsMetadata;
  fn as_view(&self) -> ::protobuf::View<'msg, RecordsMetadata> {
    *self
  }
}

impl<'msg> ::protobuf::IntoView<'msg> for RecordsMetadataView<'msg> {
  fn into_view<'shorter>(self) -> RecordsMetadataView<'shorter>
  where
      'msg: 'shorter {
    self
  }
}

impl<'msg> ::protobuf::IntoProxied<RecordsMetadata> for RecordsMetadataView<'msg> {
  fn into_proxied(self, _private: ::protobuf::__internal::Private) -> RecordsMetadata {
    let mut dst = RecordsMetadata::new();
    assert!(unsafe {
      dst.inner.ptr_mut().deep_copy(self.inner.ptr(), dst.inner.arena())
    });
    dst
  }
}

impl<'msg> ::protobuf::IntoProxied<RecordsMetadata> for RecordsMetadataMut<'msg> {
  fn into_proxied(self, _private: ::protobuf::__internal::Private) -> RecordsMetadata {
    ::protobuf::IntoProxied::into_proxied(::protobuf::IntoView::into_view(self), _private)
  }
}

impl ::protobuf::__internal::runtime::EntityType for RecordsMetadata {
    type Tag = ::protobuf::__internal::runtime::MessageTag;
}

impl<'msg> ::protobuf::__internal::runtime::EntityType for RecordsMetadataView<'msg> {
    type Tag = ::protobuf::__internal::runtime::ViewProxyTag;
}

impl<'msg> ::protobuf::__internal::runtime::EntityType for RecordsMetadataMut<'msg> {
    type Tag = ::protobuf::__internal::runtime::MutProxyTag;
}

#[allow(dead_code)]
#[allow(non_camel_case_types)]
pub struct RecordsMetadataMut<'msg> {
  inner: ::protobuf::__internal::runtime::MessageMutInner<'msg, RecordsMetadata>,
}

impl<'msg> ::protobuf::__internal::SealedInternal for RecordsMetadataMut<'msg> {}

impl<'msg> ::protobuf::MessageMut<'msg> for RecordsMetadataMut<'msg> {
  type Message = RecordsMetadata;
}

impl ::std::fmt::Debug for RecordsMetadataMut<'_> {
  fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
    write!(f, "{}", ::protobuf::__internal::runtime::debug_string(self))
  }
}

impl<'msg> From<::protobuf::__internal::runtime::MessageMutInner<'msg, RecordsMetadata>> for RecordsMetadataMut<'msg> {
  fn from(inner: ::protobuf::__internal::runtime::MessageMutInner<'msg, RecordsMetadata>) -> Self {
    Self { inner }
  }
}

#[allow(dead_code)]
impl<'msg> RecordsMetadataMut<'msg> {

  #[doc(hidden)]
  pub fn as_message_mut_inner(&mut self, _private: ::protobuf::__internal::Private)
    -> ::protobuf::__internal::runtime::MessageMutInner<'msg, RecordsMetadata> {
    self.inner
  }

  pub fn to_owned(&self) -> RecordsMetadata {
    ::protobuf::AsView::as_view(self).to_owned()
  }

  // file_comment: optional string
  pub fn has_file_comment(&self) -> bool {
    unsafe {
      self.inner.ptr().has_field_at_index(0)
    }
  }
  pub fn clear_file_comment(&mut self) {
    unsafe {
      self.inner.ptr().clear_field_at_index(
        0
      );
    }
  }
  pub fn file_comment_opt(&self) -> ::protobuf::Optional<&'_ ::protobuf::ProtoStr> {
        ::protobuf::Optional::new(self.file_comment(), self.has_file_comment())
  }
  pub fn file_comment(&self) -> ::protobuf::View<'_, ::protobuf::ProtoString> {
    let str_view = unsafe {
      self.inner.ptr().get_string_at_index(
        0, (b"").into()
      )
    };
    // SAFETY: The runtime doesn't require ProtoStr to be UTF-8.
    unsafe { ::protobuf::ProtoStr::from_utf8_unchecked(str_view.as_ref()) }
  }
  pub fn set_file_comment(&mut self, val: impl ::protobuf::IntoProxied<::protobuf::ProtoString>) {
    unsafe {
      ::protobuf::__internal::runtime::message_set_string_field(
        ::protobuf::AsMut::as_mut(self).inner,
        0,
        val);
    }
  }

  // record_type_name: optional string
  pub fn has_record_type_name(&self) -> bool {
    unsafe {
      self.inner.ptr().has_field_at_index(1)
    }
  }
  pub fn clear_record_type_name(&mut self) {
    unsafe {
      self.inner.ptr().clear_field_at_index(
        1
      );
    }
  }
  pub fn record_type_name_opt(&self) -> ::protobuf::Optional<&'_ ::protobuf::ProtoStr> {
        ::protobuf::Optional::new(self.record_type_name(), self.has_record_type_name())
  }
  pub fn record_type_name(&self) -> ::protobuf::View<'_, ::protobuf::ProtoString> {
    let str_view = unsafe {
      self.inner.ptr().get_string_at_index(
        1, (b"").into()
      )
    };
    // SAFETY: The runtime doesn't require ProtoStr to be UTF-8.
    unsafe { ::protobuf::ProtoStr::from_utf8_unchecked(str_view.as_ref()) }
  }
  pub fn set_record_type_name(&mut self, val: impl ::protobuf::IntoProxied<::protobuf::ProtoString>) {
    unsafe {
      ::protobuf::__internal::runtime::message_set_string_field(
        ::protobuf::AsMut::as_mut(self).inner,
        1,
        val);
    }
  }

  // file_descriptor: repeated message google.protobuf.FileDescriptorProto
  pub fn file_descriptor(&self) -> ::protobuf::RepeatedView<'_, super::FileDescriptorProto> {
    unsafe {
      self.inner.ptr().get_array_at_index(
        2
      )
    }.map_or_else(
        ::protobuf::__internal::runtime::empty_array::<super::FileDescriptorProto>,
        |raw| unsafe {
          ::protobuf::RepeatedView::from_raw(::protobuf::__internal::Private, raw)
        }
      )
  }
  pub fn file_descriptor_mut(&mut self) -> ::protobuf::RepeatedMut<'_, super::FileDescriptorProto> {
    unsafe {
      let raw_array = self.inner.ptr_mut().get_or_create_mutable_array_at_index(
        2,
        self.inner.arena()
      ).expect("alloc should not fail");
      ::protobuf::RepeatedMut::from_inner(
        ::protobuf::__internal::Private,
        ::protobuf::__internal::runtime::InnerRepeatedMut::new(
          raw_array, self.inner.arena(),
        ),
      )
    }
  }
  pub fn set_file_descriptor(&mut self, src: impl ::protobuf::IntoProxied<::protobuf::Repeated<super::FileDescriptorProto>>) {
    unsafe {
      ::protobuf::__internal::runtime::message_set_repeated_field(
        ::protobuf::AsMut::as_mut(self).inner,
        2,
        src);
    }
  }

  // record_writer_options: optional string
  pub fn has_record_writer_options(&self) -> bool {
    unsafe {
      self.inner.ptr().has_field_at_index(3)
    }
  }
  pub fn clear_record_writer_options(&mut self) {
    unsafe {
      self.inner.ptr().clear_field_at_index(
        3
      );
    }
  }
  pub fn record_writer_options_opt(&self) -> ::protobuf::Optional<&'_ ::protobuf::ProtoStr> {
        ::protobuf::Optional::new(self.record_writer_options(), self.has_record_writer_options())
  }
  pub fn record_writer_options(&self) -> ::protobuf::View<'_, ::protobuf::ProtoString> {
    let str_view = unsafe {
      self.inner.ptr().get_string_at_index(
        3, (b"").into()
      )
    };
    // SAFETY: The runtime doesn't require ProtoStr to be UTF-8.
    unsafe { ::protobuf::ProtoStr::from_utf8_unchecked(str_view.as_ref()) }
  }
  pub fn set_record_writer_options(&mut self, val: impl ::protobuf::IntoProxied<::protobuf::ProtoString>) {
    unsafe {
      ::protobuf::__internal::runtime::message_set_string_field(
        ::protobuf::AsMut::as_mut(self).inner,
        3,
        val);
    }
  }

  // num_records: optional int64
  pub fn has_num_records(&self) -> bool {
    unsafe {
      self.inner.ptr().has_field_at_index(4)
    }
  }
  pub fn clear_num_records(&mut self) {
    unsafe {
      self.inner.ptr().clear_field_at_index(
        4
      );
    }
  }
  pub fn num_records_opt(&self) -> ::protobuf::Optional<i64> {
        ::protobuf::Optional::new(self.num_records(), self.has_num_records())
  }
  pub fn num_records(&self) -> i64 {
    unsafe {
      // TODO: b/361751487: This .into() and .try_into() is only
      // here for the enum<->i32 case, we should avoid it for
      // other primitives where the types naturally match
      // perfectly (and do an unchecked conversion for
      // i32->enum types, since even for closed enums we trust
      // upb to only return one of the named values).
      self.inner.ptr().get_i64_at_index(
        4, (0i64).into()
      ).try_into().unwrap()
    }
  }
  pub fn set_num_records(&mut self, val: i64) {
    unsafe {
      // TODO: b/361751487: This .into() is only here
      // here for the enum<->i32 case, we should avoid it for
      // other primitives where the types naturally match
      //perfectly.
      self.inner.ptr_mut().set_base_field_i64_at_index(
        4, val.into()
      )
    }
  }

}

// SAFETY:
// - `RecordsMetadataMut` does not perform any shared mutation.
unsafe impl Send for RecordsMetadataMut<'_> {}

// SAFETY:
// - `RecordsMetadataMut` does not perform any shared mutation.
unsafe impl Sync for RecordsMetadataMut<'_> {}

impl<'msg> ::protobuf::AsView for RecordsMetadataMut<'msg> {
  type Proxied = RecordsMetadata;
  fn as_view(&self) -> ::protobuf::View<'_, RecordsMetadata> {
    RecordsMetadataView {
      inner: ::protobuf::__internal::runtime::MessageViewInner::view_of_mut(self.inner)
    }
  }
}

impl<'msg> ::protobuf::IntoView<'msg> for RecordsMetadataMut<'msg> {
  fn into_view<'shorter>(self) -> ::protobuf::View<'shorter, RecordsMetadata>
  where
      'msg: 'shorter {
    RecordsMetadataView {
      inner: ::protobuf::__internal::runtime::MessageViewInner::view_of_mut(self.inner)
    }
  }
}

impl<'msg> ::protobuf::AsMut for RecordsMetadataMut<'msg> {
  type MutProxied = RecordsMetadata;
  fn as_mut(&mut self) -> RecordsMetadataMut<'msg> {
    RecordsMetadataMut { inner: self.inner }
  }
}

impl<'msg> ::protobuf::IntoMut<'msg> for RecordsMetadataMut<'msg> {
  fn into_mut<'shorter>(self) -> RecordsMetadataMut<'shorter>
  where
      'msg: 'shorter {
    self
  }
}

#[allow(dead_code)]
impl RecordsMetadata {
  pub fn new() -> Self {
    Self { inner: ::protobuf::__internal::runtime::OwnedMessageInner::<Self>::new() }
  }


  #[doc(hidden)]
  pub fn as_message_mut_inner(&mut self, _private: ::protobuf::__internal::Private) -> ::protobuf::__internal::runtime::MessageMutInner<'_, RecordsMetadata> {
    ::protobuf::__internal::runtime::MessageMutInner::mut_of_owned(&mut self.inner)
  }

  pub fn as_view(&self) -> RecordsMetadataView<'_> {
    ::protobuf::__internal::runtime::MessageViewInner::view_of_owned(&self.inner).into()
  }

  pub fn as_mut(&mut self) -> RecordsMetadataMut<'_> {
    ::protobuf::__internal::runtime::MessageMutInner::mut_of_owned(&mut self.inner).into()
  }

  // file_comment: optional string
  pub fn has_file_comment(&self) -> bool {
    unsafe {
      self.inner.ptr().has_field_at_index(0)
    }
  }
  pub fn clear_file_comment(&mut self) {
    unsafe {
      self.inner.ptr().clear_field_at_index(
        0
      );
    }
  }
  pub fn file_comment_opt(&self) -> ::protobuf::Optional<&'_ ::protobuf::ProtoStr> {
        ::protobuf::Optional::new(self.file_comment(), self.has_file_comment())
  }
  pub fn file_comment(&self) -> ::protobuf::View<'_, ::protobuf::ProtoString> {
    let str_view = unsafe {
      self.inner.ptr().get_string_at_index(
        0, (b"").into()
      )
    };
    // SAFETY: The runtime doesn't require ProtoStr to be UTF-8.
    unsafe { ::protobuf::ProtoStr::from_utf8_unchecked(str_view.as_ref()) }
  }
  pub fn set_file_comment(&mut self, val: impl ::protobuf::IntoProxied<::protobuf::ProtoString>) {
    unsafe {
      ::protobuf::__internal::runtime::message_set_string_field(
        ::protobuf::AsMut::as_mut(self).inner,
        0,
        val);
    }
  }

  // record_type_name: optional string
  pub fn has_record_type_name(&self) -> bool {
    unsafe {
      self.inner.ptr().has_field_at_index(1)
    }
  }
  pub fn clear_record_type_name(&mut self) {
    unsafe {
      self.inner.ptr().clear_field_at_index(
        1
      );
    }
  }
  pub fn record_type_name_opt(&self) -> ::protobuf::Optional<&'_ ::protobuf::ProtoStr> {
        ::protobuf::Optional::new(self.record_type_name(), self.has_record_type_name())
  }
  pub fn record_type_name(&self) -> ::protobuf::View<'_, ::protobuf::ProtoString> {
    let str_view = unsafe {
      self.inner.ptr().get_string_at_index(
        1, (b"").into()
      )
    };
    // SAFETY: The runtime doesn't require ProtoStr to be UTF-8.
    unsafe { ::protobuf::ProtoStr::from_utf8_unchecked(str_view.as_ref()) }
  }
  pub fn set_record_type_name(&mut self, val: impl ::protobuf::IntoProxied<::protobuf::ProtoString>) {
    unsafe {
      ::protobuf::__internal::runtime::message_set_string_field(
        ::protobuf::AsMut::as_mut(self).inner,
        1,
        val);
    }
  }

  // file_descriptor: repeated message google.protobuf.FileDescriptorProto
  pub fn file_descriptor(&self) -> ::protobuf::RepeatedView<'_, super::FileDescriptorProto> {
    unsafe {
      self.inner.ptr().get_array_at_index(
        2
      )
    }.map_or_else(
        ::protobuf::__internal::runtime::empty_array::<super::FileDescriptorProto>,
        |raw| unsafe {
          ::protobuf::RepeatedView::from_raw(::protobuf::__internal::Private, raw)
        }
      )
  }
  pub fn file_descriptor_mut(&mut self) -> ::protobuf::RepeatedMut<'_, super::FileDescriptorProto> {
    unsafe {
      let raw_array = self.inner.ptr_mut().get_or_create_mutable_array_at_index(
        2,
        self.inner.arena()
      ).expect("alloc should not fail");
      ::protobuf::RepeatedMut::from_inner(
        ::protobuf::__internal::Private,
        ::protobuf::__internal::runtime::InnerRepeatedMut::new(
          raw_array, self.inner.arena(),
        ),
      )
    }
  }
  pub fn set_file_descriptor(&mut self, src: impl ::protobuf::IntoProxied<::protobuf::Repeated<super::FileDescriptorProto>>) {
    unsafe {
      ::protobuf::__internal::runtime::message_set_repeated_field(
        ::protobuf::AsMut::as_mut(self).inner,
        2,
        src);
    }
  }

  // record_writer_options: optional string
  pub fn has_record_writer_options(&self) -> bool {
    unsafe {
      self.inner.ptr().has_field_at_index(3)
    }
  }
  pub fn clear_record_writer_options(&mut self) {
    unsafe {
      self.inner.ptr().clear_field_at_index(
        3
      );
    }
  }
  pub fn record_writer_options_opt(&self) -> ::protobuf::Optional<&'_ ::protobuf::ProtoStr> {
        ::protobuf::Optional::new(self.record_writer_options(), self.has_record_writer_options())
  }
  pub fn record_writer_options(&self) -> ::protobuf::View<'_, ::protobuf::ProtoString> {
    let str_view = unsafe {
      self.inner.ptr().get_string_at_index(
        3, (b"").into()
      )
    };
    // SAFETY: The runtime doesn't require ProtoStr to be UTF-8.
    unsafe { ::protobuf::ProtoStr::from_utf8_unchecked(str_view.as_ref()) }
  }
  pub fn set_record_writer_options(&mut self, val: impl ::protobuf::IntoProxied<::protobuf::ProtoString>) {
    unsafe {
      ::protobuf::__internal::runtime::message_set_string_field(
        ::protobuf::AsMut::as_mut(self).inner,
        3,
        val);
    }
  }

  // num_records: optional int64
  pub fn has_num_records(&self) -> bool {
    unsafe {
      self.inner.ptr().has_field_at_index(4)
    }
  }
  pub fn clear_num_records(&mut self) {
    unsafe {
      self.inner.ptr().clear_field_at_index(
        4
      );
    }
  }
  pub fn num_records_opt(&self) -> ::protobuf::Optional<i64> {
        ::protobuf::Optional::new(self.num_records(), self.has_num_records())
  }
  pub fn num_records(&self) -> i64 {
    unsafe {
      // TODO: b/361751487: This .into() and .try_into() is only
      // here for the enum<->i32 case, we should avoid it for
      // other primitives where the types naturally match
      // perfectly (and do an unchecked conversion for
      // i32->enum types, since even for closed enums we trust
      // upb to only return one of the named values).
      self.inner.ptr().get_i64_at_index(
        4, (0i64).into()
      ).try_into().unwrap()
    }
  }
  pub fn set_num_records(&mut self, val: i64) {
    unsafe {
      // TODO: b/361751487: This .into() is only here
      // here for the enum<->i32 case, we should avoid it for
      // other primitives where the types naturally match
      //perfectly.
      self.inner.ptr_mut().set_base_field_i64_at_index(
        4, val.into()
      )
    }
  }

}  // impl RecordsMetadata

impl ::std::ops::Drop for RecordsMetadata {
  #[inline]
  fn drop(&mut self) {
  }
}

impl ::std::clone::Clone for RecordsMetadata {
  fn clone(&self) -> Self {
    self.as_view().to_owned()
  }
}

impl ::protobuf::AsView for RecordsMetadata {
  type Proxied = Self;
  fn as_view(&self) -> RecordsMetadataView<'_> {
    self.as_view()
  }
}

impl ::protobuf::AsMut for RecordsMetadata {
  type MutProxied = Self;
  fn as_mut(&mut self) -> RecordsMetadataMut<'_> {
    self.as_mut()
  }
}

unsafe impl ::protobuf::__internal::runtime::AssociatedMiniTable for RecordsMetadata {
  fn mini_table() -> ::protobuf::__internal::runtime::MiniTablePtr {
    static ONCE_LOCK: ::std::sync::OnceLock<::protobuf::__internal::runtime::MiniTableInitPtr> =
        ::std::sync::OnceLock::new();
    unsafe {
      ONCE_LOCK.get_or_init(|| {
        super::riegeli__RecordsMetadata_msg_init.0 =
            ::protobuf::__internal::runtime::build_mini_table("$P11G1+");
        ::protobuf::__internal::runtime::link_mini_table(
            super::riegeli__RecordsMetadata_msg_init.0, &[<super::FileDescriptorProto as ::protobuf::__internal::runtime::AssociatedMiniTable>::mini_table(),
            ], &[]);
        ::protobuf::__internal::runtime::MiniTableInitPtr(super::riegeli__RecordsMetadata_msg_init.0)
      }).0
    }
  }
}
unsafe impl ::protobuf::__internal::runtime::UpbGetArena for RecordsMetadata {
  fn get_arena(&mut self, _private: ::protobuf::__internal::Private) -> &::protobuf::__internal::runtime::Arena {
    self.inner.arena()
  }
}

unsafe impl ::protobuf::__internal::runtime::UpbGetMessagePtrMut for RecordsMetadata {
  type Msg = RecordsMetadata;
  fn get_ptr_mut(&mut self, _private: ::protobuf::__internal::Private) -> ::protobuf::__internal::runtime::MessagePtr<RecordsMetadata> {
    self.inner.ptr_mut()
  }
}
unsafe impl ::protobuf::__internal::runtime::UpbGetMessagePtr for RecordsMetadata {
  type Msg = RecordsMetadata;
  fn get_ptr(&self, _private: ::protobuf::__internal::Private) -> ::protobuf::__internal::runtime::MessagePtr<RecordsMetadata> {
    self.inner.ptr()
  }
}
unsafe impl ::protobuf::__internal::runtime::UpbGetMessagePtrMut for RecordsMetadataMut<'_> {
  type Msg = RecordsMetadata;
  fn get_ptr_mut(&mut self, _private: ::protobuf::__internal::Private) -> ::protobuf::__internal::runtime::MessagePtr<RecordsMetadata> {
    self.inner.ptr_mut()
  }
}
unsafe impl ::protobuf::__internal::runtime::UpbGetMessagePtr for RecordsMetadataMut<'_> {
  type Msg = RecordsMetadata;
  fn get_ptr(&self, _private: ::protobuf::__internal::Private) -> ::protobuf::__internal::runtime::MessagePtr<RecordsMetadata> {
    self.inner.ptr()
  }
}
unsafe impl ::protobuf::__internal::runtime::UpbGetMessagePtr for RecordsMetadataView<'_> {
  type Msg = RecordsMetadata;
  fn get_ptr(&self, _private: ::protobuf::__internal::Private) -> ::protobuf::__internal::runtime::MessagePtr<RecordsMetadata> {
    self.inner.ptr()
  }
}

unsafe impl ::protobuf::__internal::runtime::UpbGetArena for RecordsMetadataMut<'_> {
  fn get_arena(&mut self, _private: ::protobuf::__internal::Private) -> &::protobuf::__internal::runtime::Arena {
    self.inner.arena()
  }
}



