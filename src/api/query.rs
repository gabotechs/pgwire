use std::fmt::Debug;
use std::sync::Arc;

use async_trait::async_trait;
use futures::sink::{Sink, SinkExt};
use futures::stream;

use super::portal::Portal;
use super::stmt::Statement;
use super::{ClientInfo, DEFAULT_NAME};
use crate::error::{PgWireError, PgWireResult};
use crate::messages::data::{DataRow, RowDescription};
use crate::messages::extendedquery::{
    Bind, Close, Describe, Execute, Parse, Sync as PgSync, TARGET_TYPE_BYTE_PORTAL,
    TARGET_TYPE_BYTE_STATEMENT,
};
use crate::messages::response::{CommandComplete, ErrorResponse, ReadyForQuery, READY_STATUS_IDLE};
use crate::messages::simplequery::Query;
use crate::messages::PgWireBackendMessage;

/// handler for processing simple query.
#[async_trait]
pub trait SimpleQueryHandler: Send + Sync {
    ///
    async fn on_query<C>(&self, client: &mut C, query: &Query) -> PgWireResult<()>
    where
        C: ClientInfo + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        client.set_state(super::PgWireConnectionState::QueryInProgress);
        let resp = self.do_query(client, query.query()).await?;
        match resp {
            QueryResponse::Data(row_description, data_rows, status) => {
                let msgs = vec![PgWireBackendMessage::RowDescription(row_description)]
                    .into_iter()
                    .chain(data_rows.into_iter().map(PgWireBackendMessage::DataRow))
                    .chain(
                        vec![
                            PgWireBackendMessage::CommandComplete(status),
                            PgWireBackendMessage::ReadyForQuery(ReadyForQuery::new(
                                READY_STATUS_IDLE,
                            )),
                        ]
                        .into_iter(),
                    )
                    .map(Ok);

                let mut msg_stream = stream::iter(msgs);
                client.send_all(&mut msg_stream).await?;
            }
            QueryResponse::Empty(status) => {
                client
                    .feed(PgWireBackendMessage::CommandComplete(status))
                    .await?;
                client
                    .feed(PgWireBackendMessage::ReadyForQuery(ReadyForQuery::new(
                        READY_STATUS_IDLE,
                    )))
                    .await?;
                client.flush().await?;
            }
            QueryResponse::Error(e) => {
                client.feed(PgWireBackendMessage::ErrorResponse(e)).await?;
                client
                    .feed(PgWireBackendMessage::ReadyForQuery(ReadyForQuery::new(
                        READY_STATUS_IDLE,
                    )))
                    .await?;
                client.flush().await?;
            }
        }

        client.set_state(super::PgWireConnectionState::ReadyForQuery);
        Ok(())
    }

    ///
    async fn do_query<C>(&self, client: &C, query: &str) -> PgWireResult<QueryResponse>
    where
        C: ClientInfo + Unpin + Send + Sync;
}

/// Query response types:
///
/// * Data: the response contains data rows,
/// * Empty: the response has no data, like update/delete/insert
/// * Error: an error response
pub enum QueryResponse {
    Data(RowDescription, Vec<DataRow>, CommandComplete),
    Empty(CommandComplete),
    Error(ErrorResponse),
}

#[async_trait]
pub trait ExtendedQueryHandler: Send + Sync {
    async fn on_parse<C>(&self, client: &mut C, message: &Parse) -> PgWireResult<()>
    where
        C: ClientInfo + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        let stmt = Statement::from(message);
        let id = stmt.id().clone();
        client.stmt_store_mut().put(&id, Arc::new(stmt));

        Ok(())
    }

    async fn on_bind<C>(&self, client: &mut C, message: &Bind) -> PgWireResult<()>
    where
        C: ClientInfo + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        let portal = Portal::try_new(message, client)?;
        let id = portal.name().clone();
        client.portal_store_mut().put(&id, Arc::new(portal));

        Ok(())
    }

    async fn on_execute<C>(&self, client: &mut C, message: &Execute) -> PgWireResult<()>
    where
        C: ClientInfo + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        let portal_name = message.name().as_ref().map_or(DEFAULT_NAME, String::as_str);
        let store = client.portal_store();
        if let Some(portal) = store.get(portal_name) {
            match self.do_query(client, portal.as_ref()).await? {
                QueryResponse::Data(head, rows, tail) => {
                    if portal.row_description_requested() {
                        client
                            .send(PgWireBackendMessage::RowDescription(head))
                            .await?;
                    }

                    if !rows.is_empty() {
                        client
                            .send_all(&mut stream::iter(
                                rows.into_iter()
                                    .map(|r| Ok(PgWireBackendMessage::DataRow(r))),
                            ))
                            .await?;
                    }

                    client
                        .send(PgWireBackendMessage::CommandComplete(tail))
                        .await?;
                }
                QueryResponse::Empty(tail) => {
                    client
                        .send(PgWireBackendMessage::CommandComplete(tail))
                        .await?;
                }
                QueryResponse::Error(err) => {
                    client
                        .send(PgWireBackendMessage::ErrorResponse(err))
                        .await?;
                }
            }

            Ok(())
        } else {
            Err(PgWireError::PortalNotFound(portal_name.to_owned()))
        }
        // TODO: clear/remove portal?
    }

    async fn on_describe<C>(&self, client: &mut C, message: &Describe) -> PgWireResult<()>
    where
        C: ClientInfo + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        let portal_name = message.name().as_ref().map_or(DEFAULT_NAME, String::as_str);
        if let Some(mut portal) = client.portal_store().get(portal_name) {
            // TODO: check if make_mut works for this
            Arc::make_mut(&mut portal).set_row_description_requested(true);
            client.portal_store_mut().put(portal_name, portal);
            Ok(())
        } else {
            Err(PgWireError::PortalNotFound(portal_name.to_owned()))
        }
    }

    async fn on_sync<C>(&self, client: &mut C, _message: &PgSync) -> PgWireResult<()>
    where
        C: ClientInfo + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        client.flush().await?;
        Ok(())
    }

    async fn on_close<C>(&self, client: &mut C, message: &Close) -> PgWireResult<()>
    where
        C: ClientInfo + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        let name = message.name().as_ref().map_or(DEFAULT_NAME, String::as_str);
        match message.target_type() {
            TARGET_TYPE_BYTE_STATEMENT => {
                client.stmt_store_mut().del(name);
            }
            TARGET_TYPE_BYTE_PORTAL => {
                client.portal_store_mut().del(name);
            }
            _ => {}
        }
        Ok(())
    }

    async fn do_query<C>(&self, client: &mut C, portal: &Portal) -> PgWireResult<QueryResponse>
    where
        C: ClientInfo + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>;
}
