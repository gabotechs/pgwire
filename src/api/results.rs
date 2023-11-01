use std::{fmt::Debug, sync::Arc};

use bytes::BytesMut;
use futures::{
    stream::{BoxStream, StreamExt},
    Stream,
};
use postgres_types::{IsNull, ToSql, Type};

use crate::{
    error::{ErrorInfo, PgWireResult},
    messages::{
        data::{DataRow, FieldDescription, RowDescription, FORMAT_CODE_BINARY, FORMAT_CODE_TEXT},
        response::CommandComplete,
    },
    types::ToSqlText,
};

#[derive(Debug, Eq, PartialEq)]
pub struct Tag {
    command: String,
    rows: Option<usize>,
}

impl Tag {
    pub fn new_for_query(rows: usize) -> Tag {
        Tag {
            command: "SELECT".to_owned(),
            rows: Some(rows),
        }
    }

    pub fn new_for_execution(command: &str, rows: Option<usize>) -> Tag {
        Tag {
            command: command.to_owned(),
            rows,
        }
    }
}

impl From<Tag> for CommandComplete {
    fn from(tag: Tag) -> CommandComplete {
        let tag_string = if let Some(rows) = tag.rows {
            format!("{} {rows}", tag.command)
        } else {
            tag.command
        };
        CommandComplete::new(tag_string)
    }
}

/// Describe encoding of a data field.
#[derive(Debug, Eq, PartialEq, Clone, Copy)]
pub enum FieldFormat {
    Text,
    Binary,
}

impl FieldFormat {
    /// Get format code for the encoding.
    pub fn value(&self) -> i16 {
        match self {
            Self::Text => FORMAT_CODE_TEXT,
            Self::Binary => FORMAT_CODE_BINARY,
        }
    }

    /// Parse FieldFormat from format code.
    ///
    /// 0 for text format, 1 for binary format. If the input is neither 0 nor 1,
    /// here we return text as default value.
    pub fn from(code: i16) -> Self {
        if code == FORMAT_CODE_BINARY {
            FieldFormat::Binary
        } else {
            FieldFormat::Text
        }
    }
}

#[derive(Debug, new, Eq, PartialEq, Clone, Getters)]
#[getset(get = "pub")]
pub struct FieldInfo {
    name: String,
    table_id: Option<i32>,
    column_id: Option<i16>,
    datatype: Type,
    type_size: Option<i16>,
    type_modifier: Option<i32>,
    format: FieldFormat,
}

impl From<&FieldInfo> for FieldDescription {
    fn from(fi: &FieldInfo) -> Self {
        FieldDescription::new(
            fi.name.clone(),           // name
            fi.table_id.unwrap_or(0),  // table_id
            fi.column_id.unwrap_or(0), // column_id
            fi.datatype.oid(),         // type_id
            fi.type_size.unwrap_or(0),
            fi.type_modifier.unwrap_or(0),
            fi.format.value(),
        )
    }
}

pub(crate) fn into_row_description(fields: &[FieldInfo]) -> RowDescription {
    RowDescription::new(fields.iter().map(Into::into).collect())
}

pub struct QueryResponse<'a> {
    row_schema: Arc<Vec<FieldInfo>>,
    data_rows: BoxStream<'a, PgWireResult<DataRow>>,
}

impl<'a> QueryResponse<'a> {
    /// Create `QueryResponse` from column schemas and stream of data row
    pub fn new<S>(field_defs: Arc<Vec<FieldInfo>>, row_stream: S) -> QueryResponse<'a>
    where
        S: Stream<Item = PgWireResult<DataRow>> + Send + Unpin + 'a,
    {
        QueryResponse {
            row_schema: field_defs,
            data_rows: row_stream.boxed(),
        }
    }

    /// Get schema of columns
    pub fn row_schema(&self) -> Arc<Vec<FieldInfo>> {
        self.row_schema.clone()
    }

    /// Get owned `BoxStream` of data rows
    pub fn data_rows(self) -> BoxStream<'a, PgWireResult<DataRow>> {
        self.data_rows
    }
}

pub struct DataRowEncoder {
    buffer: DataRow,
    field_buffer: BytesMut,
    schema: Arc<Vec<FieldInfo>>,
    col_index: usize,
}

impl DataRowEncoder {
    /// New DataRowEncoder from schemas of column
    pub fn new(fields: Arc<Vec<FieldInfo>>) -> DataRowEncoder {
        let ncols = fields.len();
        Self {
            buffer: DataRow::new(Vec::with_capacity(ncols)),
            field_buffer: BytesMut::with_capacity(8),
            schema: fields,
            col_index: 0,
        }
    }

    /// Encode value with custom type and format
    ///
    /// This encode function ignores data type and format information from
    /// schema of this encoder.
    pub fn encode_field_with_type_and_format<T>(
        &mut self,
        value: &T,
        data_type: &Type,
        format: FieldFormat,
    ) -> PgWireResult<()>
    where
        T: ToSql + ToSqlText + Sized,
    {
        let is_null = if format == FieldFormat::Text {
            value.to_sql_text(data_type, &mut self.field_buffer)?
        } else {
            value.to_sql(data_type, &mut self.field_buffer)?
        };

        if let IsNull::No = is_null {
            let buf = self.field_buffer.split().freeze();
            self.buffer.fields_mut().push(Some(buf));
        } else {
            self.buffer.fields_mut().push(None);
        }

        self.col_index += 1;
        self.field_buffer.clear();
        Ok(())
    }

    /// Encode value using type and format, defined by schema
    ///
    /// Panic when encoding more columns than provided as schema.
    pub fn encode_field<T>(&mut self, value: &T) -> PgWireResult<()>
    where
        T: ToSql + ToSqlText + Sized,
    {
        let data_type = self.schema[self.col_index].datatype();
        let format = self.schema[self.col_index].format();

        let is_null = if *format == FieldFormat::Text {
            value.to_sql_text(data_type, &mut self.field_buffer)?
        } else {
            value.to_sql(data_type, &mut self.field_buffer)?
        };

        if let IsNull::No = is_null {
            let buf = self.field_buffer.split().freeze();
            self.buffer.fields_mut().push(Some(buf));
        } else {
            self.buffer.fields_mut().push(None);
        }

        self.col_index += 1;
        self.field_buffer.clear();
        Ok(())
    }

    pub fn finish(mut self) -> PgWireResult<DataRow> {
        self.col_index = 0;
        Ok(self.buffer)
    }
}

/// Response for frontend describe requests.
///
/// There are two types of describe: statement and portal. When describing
/// statement, frontend expects parameter types inferenced by server. And both
/// describe messages will require column definitions for resultset being
/// returned.
#[derive(Debug, Getters, new)]
#[getset(get = "pub")]
pub struct DescribeResponse {
    parameters: Option<Vec<Type>>,
    fields: Vec<FieldInfo>,
}

impl DescribeResponse {
    /// Create an no_data instance of `DescribeResponse`. This is typically used
    /// when client tries to describe an empty query.
    pub fn no_data() -> Self {
        DescribeResponse {
            parameters: None,
            fields: vec![],
        }
    }

    /// Return true if the `DescribeResponse` is empty/nodata
    pub fn is_no_data(&self) -> bool {
        self.parameters.is_none() && self.fields.is_empty()
    }
}

/// Query response types:
///
/// * Query: the response contains data rows
/// * Execution: response for ddl/dml execution
/// * Error: error response
pub enum Response<'a> {
    EmptyQuery,
    Query(QueryResponse<'a>),
    Execution(Tag),
    Error(Box<ErrorInfo>),
}

#[cfg(test)]
mod test {
    use std::time::SystemTime;

    use super::*;

    #[test]
    fn test_command_complete() {
        let tag = Tag::new_for_execution("INSERT", Some(100));
        let cc = CommandComplete::from(tag);

        assert_eq!(cc.tag(), "INSERT 100");
    }

    #[test]
    fn test_data_row_encoder() {
        let schema = Arc::new(vec![
            FieldInfo::new("id".into(), None, None, Type::INT4, None, None, FieldFormat::Text),
            FieldInfo::new("name".into(), None, None, Type::VARCHAR, None, None, FieldFormat::Text),
            FieldInfo::new("ts".into(), None, None, Type::TIMESTAMP, None, None, FieldFormat::Text),
        ]);
        let mut encoder = DataRowEncoder::new(schema);
        encoder.encode_field(&2001).unwrap();
        encoder.encode_field(&"udev").unwrap();
        encoder.encode_field(&SystemTime::now()).unwrap();

        let row = encoder.finish().unwrap();

        assert_eq!(row.fields().len(), 3);
        assert_eq!(row.fields()[0].as_ref().unwrap().len(), 4);
        assert_eq!(row.fields()[1].as_ref().unwrap().len(), 4);
        assert_eq!(row.fields()[2].as_ref().unwrap().len(), 26);
    }
}
