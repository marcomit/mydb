//! A tuple is a single database row or any collection of [`Value`] instances.
//!
//! TODO: Right now we're "serializing" and "deserializing" rows which is not
//! really needed. We already store the rows in such a format that we can
//! interpret the bytes in them as numbers or strings without having to copy
//! them into [`Value`] structures. Serializing and deserializing made it easy
//! to develop in the beginning because it doesn't require any unsafe code, but
//! it's probably the biggest performance hit not counting unoptimized IO.
//!
//! # Serialization Format
//!
//! All numbers are serialized in big endian format because that allows the
//! BTree to compare them using a simple memcmp(). Normally only the first
//! column of a tuple needs to be compared as that's where we store the
//! primary key or [`RowId`], but for simplicity we just encode every number in
//! big endian. This avoids the case "if number is PK then big endian else
//! little endian". But that's what we *should* do (laziness wins again).
//!
//! Strings on the other hand are UTF-8 encoded with a 1, 2 or 4 byte little
//! endian prefix where we store the byte length of the string (number of bytes,
//! not number of characters). The amount of bytes required to store the length
//! depends on the maximum character limit defined by `VARCHAR` types. See
//! [`utf8_length_prefix_bytes`] for details. So, putting it all together, a
//! tuple like this one:
//!
//! ```ignore
//! [
//!     Value::Number(1),
//!     Value::String("hello".into()),
//!     Value::Number(2),
//! ]
//! ```
//!
//! with a schema like this one:
//!
//! ```ignore
//! [DataType::BigInt, DataType::Varchar(255), DataType::Int]
//! ```
//!
//! would serialize into the following bytes (not bits, bytes):
//!
//! ```text
//! +-----------------+-----+---------------------+---------+
//! | 0 0 0 0 0 0 0 1 | 5 0 | 'h' 'e' 'l' 'l' 'o' | 0 0 0 2 |
//! +-----------------+-----+---------------------+---------+
//!      8 byte        2 byte    String bytes       4 byte
//!    big endian      little                     big endian
//!      BigInt        endian                         Int
//!                    String
//!                    length
//! ```
//!
//! The only thing we're missing here is alignment. The page module already
//! supports 64 bit alignment, so if we align columns and write some unsafe
//! code to obtain references to values from a binary buffer we would get rid
//! of serialization / deserialization. It would require some changes throughout
//! the codebase, but definitely doable.
use std::{
  io::{self, Read},
  mem,
};

use crate::{
  db::{RowId, Schema},
  sql::statement::{DataType, Value},
};

/// Almost all tuples (except BTree index tuples) have a [`RowId`] as the first
/// element.
///
/// This function returns the [`RowId`].
pub(crate) fn deserialize_row_id(buf: &[u8]) -> RowId {
  RowId::from_be_bytes(buf[..mem::size_of::<RowId>()].try_into().unwrap())
}

/// Serializes the `row_id` into a big endian buffer.
pub(crate) fn serialize_row_id(row_id: RowId) -> [u8; mem::size_of::<RowId>()] {
  row_id.to_be_bytes()
}

/// Returns the byte length of the given data type. Only works with integers.
pub(crate) fn byte_length_of_integer_type(data_type: &DataType) -> usize {
  match data_type {
      DataType::Int | DataType::UnsignedInt => 4,
      DataType::BigInt | DataType::UnsignedBigInt => 8,
      _ => unreachable!("byte_length_of_integer_type() called with incorrect {data_type:?}"),
  }
}

/// Returns the number of bytes we need to store the length of a `VARCHAR` type.
///
/// UTF-8 encodes each character using anywhere from 1 to 4 bytes. So
/// considering the worst case scenario where every single character in a string
/// is 4 bytes, a `VARCHAR(255)` type would require 2 bytes to store 255
/// characters unlike ASCII which only requires 1 byte.
pub(crate) fn utf8_length_prefix_bytes(max_characters: usize) -> usize {
  match max_characters {
      0..64 => 1,
      64..16384 => 2,
      _ => 4,
  }
}

/// Checks if we can store an integer using one of the SQL [`DataType`]
/// variants.
pub(crate) fn integer_is_within_range(integer: &i128, integer_type: &DataType) -> bool {
  let bounds = match integer_type {
      DataType::Int => i32::MIN as i128..=i32::MAX as i128,
      DataType::UnsignedInt => 0..=u32::MAX as i128,
      DataType::BigInt => i64::MIN as i128..=i64::MAX as i128,
      DataType::UnsignedBigInt => 0..=u64::MAX as i128,
      other => unreachable!("is 'integer' not clear enough?: {other}"),
  };

  bounds.contains(integer)
}

/// Calculates the size that the given tuple would take on disk once serialized.
pub(crate) fn size_of(tuple: &[Value], schema: &Schema) -> usize {
  schema
      .columns
      .iter()
      .enumerate()
      .map(|(i, col)| match col.data_type {
          DataType::Bool => 1,

          DataType::Varchar(max_characters) => {
              let Value::String(string) = &tuple[i] else {
                  panic!(
                      "expected data type {}, found value {}",
                      DataType::Varchar(max_characters),
                      tuple[i]
                  );
              };

              utf8_length_prefix_bytes(max_characters) + string.as_bytes().len()
          }

          integer_type => byte_length_of_integer_type(&integer_type),
      })
      .sum()
}

/// Serialize a single value.
///
/// It's called serialize key because otherwise we just use [`serialize`].
/// This is only used to serialize the first part of a tuple in order to search
/// BTrees.
pub(crate) fn serialize_key(data_type: &DataType, value: &Value) -> Vec<u8> {
  let mut buf = Vec::new();
  serialize_value_into(&mut buf, data_type, value);
  buf
}

/// Serialize a complete tuple.
///
/// See the module level documentation for the serialization format.
pub(crate) fn serialize<'v>(
  schema: &Schema,
  values: (impl IntoIterator<Item = &'v Value> + Copy),
) -> Vec<u8> {
  let mut buf = Vec::new();

  debug_assert_eq!(
      schema.len(),
      values.into_iter().count(),
      "length of schema and values must the same",
  );

  for (col, val) in schema.columns.iter().zip(values.into_iter()) {
      serialize_value_into(&mut buf, &col.data_type, val);
  }

  buf
}

/// Low level serialization.
///
/// This one takes a reference instead of producing a new [`Vec<u8>`] because
/// that would be expensive for serializing complete tuples as we'd have to
/// allocate multiple vectors and join them together.
///
/// TODO: Alignment.
fn serialize_value_into(buf: &mut Vec<u8>, data_type: &DataType, value: &Value) {
  match (data_type, value) {
      (DataType::Varchar(max_characters), Value::String(string)) => {
          if string.as_bytes().len() > u32::MAX as usize {
              todo!("strings longer than {} bytes are not handled", u32::MAX);
          }

          let byte_length = string.as_bytes().len().to_le_bytes();
          let length_prefix_bytes = utf8_length_prefix_bytes(*max_characters);

          buf.extend_from_slice(&byte_length[..length_prefix_bytes]);
          buf.extend_from_slice(string.as_bytes());
      }

      (DataType::Bool, Value::Bool(bool)) => buf.push(u8::from(*bool)),

      (integer_type, Value::Number(num)) => {
          assert!(
              integer_is_within_range(num, integer_type),
              "integer overflow while serializing number {num} into data type {integer_type:?}"
          );

          let byte_length = byte_length_of_integer_type(integer_type);
          let big_endian_bytes = num.to_be_bytes();
          buf.extend_from_slice(&big_endian_bytes[big_endian_bytes.len() - byte_length..]);
      }

      _ => unreachable!("attempt to serialize {value} into {data_type}"),
  }
}

/// See the module level documentation for the serialization format.
pub fn deserialize(buf: &[u8], schema: &Schema) -> Vec<Value> {
  read_from(&mut io::Cursor::new(buf), schema).unwrap()
}

/// Reads one single tuple from the given reader.
///
/// This will call [`Read::read_exact`] many times so make sure the reader is
/// buffered or is an in-memory array such as [`io::Cursor<Vec<u8>>`].
///
/// TODO: Alignment.
pub fn read_from(reader: &mut impl Read, schema: &Schema) -> io::Result<Vec<Value>> {
  let values = schema.columns.iter().map(|column| {
      Ok(match column.data_type {
          DataType::Varchar(max_characters) => {
              let mut length_buffer = [0; mem::size_of::<usize>()];
              let length_prefix_bytes = utf8_length_prefix_bytes(max_characters);

              reader.read_exact(&mut length_buffer[..length_prefix_bytes])?;
              let length = usize::from_le_bytes(length_buffer);

              let mut string = vec![0; length];
              reader.read_exact(&mut string)?;

              // TODO: We can probably call from_utf8_unchecked() here.
              Value::String(String::from_utf8(string).unwrap())
          }

          DataType::Bool => {
              let mut byte = [0];
              reader.read_exact(&mut byte)?;
              Value::Bool(byte[0] != 0)
          }

          integer_type => {
              let byte_length = byte_length_of_integer_type(&integer_type);
              let mut big_endian_buf = [0; mem::size_of::<i128>()];

              let start_index = mem::size_of::<i128>() - byte_length;
              reader.read_exact(&mut big_endian_buf[start_index..])?;

              // Adjustment for negative numbers. Gotta love two's complement.
              if big_endian_buf[start_index] & 0x80 != 0
                  && matches!(integer_type, DataType::BigInt | DataType::Int)
              {
                  big_endian_buf[..start_index].fill(u8::MAX);
              }

              Value::Number(i128::from_be_bytes(big_endian_buf))
          }
      })
  });

  values.collect()
}