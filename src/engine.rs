/*
 * Copyright 2025-present ScyllaDB
 * SPDX-License-Identifier: Apache-2.0
 */

use {
    crate::{
        actor::{ActorHandle, ActorStop, MessageStop},
        index::{self, Index},
        modify_indexes::{self, ModifyIndexesExt},
        monitor_indexes, monitor_items, monitor_queries,
        supervisor::{Supervisor, SupervisorExt},
        ColumnName, Connectivity, Dimensions, ExpansionAdd, ExpansionSearch, IndexId, ScyllaDbUri,
    },
    std::{collections::HashMap, future::Future},
    tokio::sync::{mpsc, oneshot},
    tracing::{error, warn},
};

pub(crate) enum Engine {
    GetIndexes {
        tx: oneshot::Sender<Vec<IndexId>>,
    },
    AddIndex {
        id: IndexId,
        col_id: ColumnName,
        col_emb: ColumnName,
        dimensions: Dimensions,
        connectivity: Connectivity,
        expansion_add: ExpansionAdd,
        expansion_search: ExpansionSearch,
    },
    DelIndex {
        id: IndexId,
    },
    GetIndex {
        id: IndexId,
        tx: oneshot::Sender<Option<mpsc::Sender<Index>>>,
    },
    Stop,
}

impl MessageStop for Engine {
    fn message_stop() -> Self {
        Engine::Stop
    }
}

pub(crate) trait EngineExt {
    async fn get_indexes(&self) -> Vec<IndexId>;
    #[allow(clippy::too_many_arguments)] // TODO: support for table params is experimental
    async fn add_index(
        &self,
        id: IndexId,
        col_id: ColumnName,
        col_emb: ColumnName,
        dimensions: Dimensions,
        connectivity: Connectivity,
        expansion_add: ExpansionAdd,
        expansion_search: ExpansionSearch,
    );
    async fn del_index(&self, id: IndexId);
    fn get_index(&self, id: IndexId) -> impl Future<Output = Option<mpsc::Sender<Index>>> + Send;
}

impl EngineExt for mpsc::Sender<Engine> {
    async fn get_indexes(&self) -> Vec<IndexId> {
        let (tx, rx) = oneshot::channel();
        if self.send(Engine::GetIndexes { tx }).await.is_ok() {
            rx.await.unwrap_or(Vec::new())
        } else {
            Vec::new()
        }
    }

    async fn add_index(
        &self,
        id: IndexId,
        col_id: ColumnName,
        col_emb: ColumnName,
        dimensions: Dimensions,
        connectivity: Connectivity,
        expansion_add: ExpansionAdd,
        expansion_search: ExpansionSearch,
    ) {
        self.send(Engine::AddIndex {
            id,
            col_id,
            col_emb,
            dimensions,
            connectivity,
            expansion_add,
            expansion_search,
        })
        .await
        .unwrap_or_else(|err| warn!("EngineExt::add_index: unable to send request: {err}"));
    }

    async fn del_index(&self, id: IndexId) {
        self.send(Engine::DelIndex { id })
            .await
            .unwrap_or_else(|err| warn!("EngineExt::del_index: unable to send request: {err}"));
    }

    async fn get_index(&self, id: IndexId) -> Option<mpsc::Sender<Index>> {
        let (tx, rx) = oneshot::channel();
        if self.send(Engine::GetIndex { id, tx }).await.is_ok() {
            rx.await.ok().flatten()
        } else {
            None
        }
    }
}

pub(crate) async fn new(
    uri: ScyllaDbUri,
    supervisor_actor: mpsc::Sender<Supervisor>,
) -> anyhow::Result<(mpsc::Sender<Engine>, ActorHandle)> {
    let (tx, mut rx) = mpsc::channel(10);
    let (monitor_actor, monitor_task) = monitor_indexes::new(uri.clone(), tx.clone()).await?;
    supervisor_actor.attach(monitor_actor, monitor_task).await;
    let (modify_actor, modify_task) = modify_indexes::new(uri.clone()).await?;
    supervisor_actor
        .attach(modify_actor.clone(), modify_task)
        .await;
    let (monitor_actor, monitor_task) = monitor_queries::new(uri.clone(), tx.clone()).await?;
    supervisor_actor.attach(monitor_actor, monitor_task).await;
    let task = tokio::spawn(async move {
        let mut indexes = HashMap::new();
        let mut monitors = HashMap::new();
        while let Some(msg) = rx.recv().await {
            match msg {
                Engine::GetIndexes { tx } => {
                    tx.send(indexes.keys().cloned().collect())
                        .unwrap_or_else(|_| {
                            warn!("engine::Engine::GetIndexes: unable to send response")
                        });
                }
                Engine::AddIndex {
                    id,
                    col_id,
                    col_emb,
                    dimensions,
                    connectivity,
                    expansion_add,
                    expansion_search,
                } => {
                    if indexes.contains_key(&id) {
                        continue;
                    }
                    if let Ok((index_actor, index_task)) = index::new(
                        id.clone(),
                        modify_actor.clone(),
                        dimensions,
                        connectivity,
                        expansion_add,
                        expansion_search,
                    ) {
                        if let Ok((monitor_actor, monitor_task)) = monitor_items::new(
                            uri.clone(),
                            id.clone().0.into(),
                            col_id.clone(),
                            col_emb.clone(),
                            index_actor.clone(),
                        )
                        .await.inspect_err(|err| error!("unable to create monitor items with uri {uri}, table {id}, col_id {col_id}, col_emb {col_emb}: {err}"))
                        {
                            supervisor_actor
                                .attach(index_actor.clone(), index_task)
                                .await;
                            supervisor_actor
                                .attach(monitor_actor.clone(), monitor_task)
                                .await;
                            indexes.insert(id.clone(), index_actor);
                            monitors.insert(id, monitor_actor);
                        } else {
                            index_actor.actor_stop().await;
                            index_task.await.unwrap_or_else(|err| warn!("engine::Engine::AddIndex: issue while stopping index actor: {err}"));
                        }
                    } else {
                        error!("unable to create index with dimensions {dimensions}");
                    }
                }
                Engine::DelIndex { id } => {
                    if let Some(index) = indexes.remove(&id) {
                        index.actor_stop().await;
                    }
                    if let Some(monitor) = monitors.remove(&id) {
                        monitor.actor_stop().await;
                    }
                    modify_actor.del(id).await;
                }
                Engine::GetIndex { id, tx } => {
                    tx.send(indexes.get(&id).cloned()).unwrap_or_else(|_| {
                        warn!("engine::Engine::GetIndex: unable to send response")
                    });
                }
                Engine::Stop => rx.close(),
            }
        }
    });
    Ok((tx, task))
}
