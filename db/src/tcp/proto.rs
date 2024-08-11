//! Simple protocol inspired by [RESP] (Redis Serialization Protocol).
//!
//! [RESP]: https://redis.io/docs/reference/protocol-spec/
//!
//! # Client Side
//!
//! Clients can only perform 3 actions:
//!
//! - Open connection.
//! - Send one raw SQL statement (UTF-8 string) at a time.
//! - Close connection.
//!
//! The SQL statement packet must start with a 4 byte little endian integer
//! indicating the length in bytes of the SQL UTF-8 string. This header must be
//! followed by the SQL statement. Visual example:
//!
//! ```text
//!  SQL String
//!     Len           SQL String
//! +----------+-----------------------+
//! | 20 0 0 0 |  SELECT * FROM table; |
//! +----------+-----------------------+
//!   4 bytes          20 bytes
//!    Little           UTF-8
//!    Endian
//! ```
//!
//! # Server Side
//!
//! The server will execute the SQL statement that the client sends and respond
//! with one of the [`Response`] variants. The serialization format depends on
//! the variant, but there are a couple of rules applied to all of them:
//!
//! 1. All responses must include a header that contains a 4 byte little endian
//! integer which indicates the length of the payload in bytes.
//!
//! 2. All responses must start with a one byte ASCII prefix.
//!
//! ## Empty Set
//!
//! [`Response::EmptySet`] indicates that the SQL statement was executed
//! successfuly but did not produce any results. It might have affected a number
//! of rows (for example by deleting them) but there is no row/column data to
//! return. Instead, this response only contains the number of affected rows.
//!
//! The empty set payload is encoded with the prefix `!` followed by a 4 byte
//! little endian integer that stores the number of affected rows. For an empty
//! set response that affected 9 rows the complete packet would be as follows:
//!
//! ```text
//!   Payload   ASCII   Affected
//!     Len     Prefix    Rows
//! +---------+-------+---------+
//! | 5 0 0 0 |  '!'  | 9 0 0 0 |
//! +---------+-------+---------+
//!   4 bytes  1 byte   4 bytes
//!   Little   ASCII    Little
//!   Endian            Endian
//! ```
//!
//! ## Error
//!
//! All error responses are just simple UTF-8 strings. The prefix for
//! [`Response::Err`] is `-` and it's directly followed by the UTF-8 string
//! error itself. No length prefix needed since the packet only contains the
//! string. For a response error with the message `table "test" does not exist`
//! the complete packet would be:
//!
//! ```text
//!   Payload   ASCII
//!     Len     Prefix          Error Message
//! +----------+-------+-----------------------------+
//! | 28 0 0 0 |  '-'  | table "test" does not exist |
//! +----------+-------+-----------------------------+
//!   4 bytes   1 byte           27 bytes
//!   Little    ASCII             UTF-8
//!   Endian
//! ```
//!
//! ## Query Set
//!
//! [`Response::QuerySet`] includes the schema of the returned values and
//! the returned values themselves. The prefix for this variant is `+`.
//! Following the prefix there's a 2 byte little endian integer that encodes
//! the number of columns in the schema. After that, columns are serialized in
//! the following format:
//!
//! ```text
//!    Name      Column     Data
//!    Len        Name      Type
//! +--------+-------------+-----+
//! |  11 0  | column name |  1  |
//! +--------+-------------+-----+
//!  2 bytes    N bytes     1 byte
//!  Little     UTF-8
//!  Endian
//! ```
//!
//! The name of the column is a UTF-8 string prefixed by a 2 byte little endian
//! integer corresponding to its byte length (ideally columns shouldn't have
//! names longer than 65535 bytes). The final component of a column is one byte
//! that encodes the column [`DataType`] variant. The mapping is as follows:
//!
//! ```ignore
//! match col.data_type {
//!     DataType::Bool => 0,
//!     DataType::Int => 1,
//!     DataType::UnsignedInt => 2,
//!     DataType::BigInt => 3,
//!     DataType::UnsignedBigInt => 4,
//!     DataType::Varchar(_) => 5,
//! }
//! ```
//!
//! If the data type is `VARCHAR` then the character limit is encoded as 4 byte
//! little endian integer right after the data type byte:
//!
//! ```text
//!    Name      Column     Data    Varchar
//!    Len        Name      Type     Limit
//! +--------+-------------+-----+-----------+
//! |  11 0  | column name |  5  | 255 0 0 0 |
//! +--------+-------------+-----+-----------+
//!  2 bytes    N bytes    1 byte   4 bytes
//!  Little      UTF-8              Little
//!  Endian                         Endian
//! ```
//!
//! Finally, after all the columns, the response packet encodes the tuple
//! results prefixed by a 4 byte little endian integer that indicates the total
//! number of tuples. Tuples are encoded using the exact same format that we
//! use to store them in the database. Refer to [`crate::storage::tuple`] for
//! details, but in a nutshell the tuple `(1, "hello", 3)` encoded with the
//! data types `[BigInt, Varchar(255), Int]` looks like this:
//!
//! ```text
//! +-----------------+-----+---------------------+---------+
//! | 0 0 0 0 0 0 0 1 | 5 0 | 'h' 'e' 'l' 'l' 'o' | 0 0 0 2 |
//! +-----------------+-----+---------------------+---------+
//!      8 byte        2 byte    String bytes       4 byte
//!    Big Endian      Little                     Big Endian
//!      BigInt        Endian                         Int
//!                    String
//!                    Length
//! ```
//!
//! So, putting it all together and assuming that the names of the columns for
//! the data types mentioned above are `("id", "msg", "num")`, a complete packet
//! that encodes the tuples `[(1, "hello", 2), (2, "world", 4)]` would look
//! like this:
//!
//! ```text
//!   Payload   ASCII   Num of    Name            Data
//!     Len     Prefix  Columns   Len     Name    Type
//! +----------+-------+-------+-------+--------+-----+
//! | 68 0 0 0 |  '+'  |  3 0  |  2 0  |  "id"  |  3  |
//! +----------+-------+-------+-------+--------+-----+
//!   4 bytes   1 byte  4 bytes 2 bytes 2 bytes  1 byte
//!
//!   Name           Data    Varchar    Name            Data     Num
//!   Len    Name    Type     Limit     Len     Name    Type    Tuples
//! +------+-------+-----+-----------+-------+---------+-----+---------+
//! | 3 0  | "msg" |  5  | 255 0 0 0 |  3 0  |  "msg"  |  1  | 2 0 0 0 |
//! +------+-------+-----+-----------+-------+---------+-----+---------+
//! 2 bytes 3 bytes 1 byte  4 bytes   2 bytes 3 bytes  1 byte  4 bytes
//!
//!     id column            msg column            num column
//! +-----------------+-----+---------------------+---------+
//! | 0 0 0 0 0 0 0 1 | 5 0 | 'h' 'e' 'l' 'l' 'o' | 0 0 0 2 |
//! +-----------------+-----+---------------------+---------+
//!      8 bytes      2 bytes       5 bytes         4 byte
//!
//!     id column            msg column            num column
//! +-----------------+-----+---------------------+---------+
//! | 0 0 0 0 0 0 0 2 | 5 0 | 'w' 'o' 'r' 'l' 'd' | 0 0 0 4 |
//! +-----------------+-----+---------------------+---------+
//!      8 bytes      2 bytes       5 bytes         4 byte
//! ```
pub enum Response{
  QuerySet(String),
  EmptySet(String),
  Err(String),
}
