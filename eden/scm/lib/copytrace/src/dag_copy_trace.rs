/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use dag::DagAlgorithm;
use itertools::Itertools;
use manifest::DiffType;
use manifest::Manifest;
use manifest_tree::Diff;
use manifest_tree::TreeManifest;
use manifest_tree::TreeStore;
use pathhistory::RenameTracer;
use pathmatcher::AlwaysMatcher;
use storemodel::futures::StreamExt;
use storemodel::ReadFileContents;
use storemodel::ReadRootTreeIds;
use types::HgId;
use types::Key;
use types::RepoPathBuf;

use crate::error::CopyTraceError;
use crate::CopyTrace;

pub struct DagCopyTrace {
    /* Input */
    /// Resolve commit ids to trees in batch.
    root_tree_reader: Arc<dyn ReadRootTreeIds + Send + Sync>,

    /// Resolve and prefetch trees in batch.
    tree_store: Arc<dyn TreeStore + Send + Sync>,

    // Read content and rename metadata of a file
    file_reader: Arc<dyn ReadFileContents<Error = anyhow::Error> + Send + Sync>,

    /// Commit graph algorithms
    dag: Arc<dyn DagAlgorithm + Send + Sync>,
}

impl DagCopyTrace {
    #[allow(dead_code)]
    pub fn new(
        root_tree_reader: Arc<dyn ReadRootTreeIds + Send + Sync>,
        tree_store: Arc<dyn TreeStore + Send + Sync>,
        file_reader: Arc<dyn ReadFileContents<Error = anyhow::Error> + Send + Sync>,
        dag: Arc<dyn DagAlgorithm + Send + Sync>,
    ) -> Result<Self> {
        let dag_copy_trace = Self {
            root_tree_reader,
            tree_store,
            file_reader,
            dag,
        };
        Ok(dag_copy_trace)
    }

    async fn read_renamed_metadata(
        &self,
        keys: Vec<Key>,
    ) -> Result<HashMap<RepoPathBuf, RepoPathBuf>> {
        tracing::trace!(keys_len = keys.len(), " read_renamed_metadata");
        let mut renames = self.file_reader.read_rename_metadata(keys).await;

        let mut map: HashMap<RepoPathBuf, RepoPathBuf> = HashMap::new();
        while let Some(rename) = renames.next().await {
            let (key, rename_from_key) = rename?;
            if let Some(rename_from_key) = rename_from_key {
                map.insert(key.path, rename_from_key.path);
            }
        }
        tracing::trace!(result_map_len = map.len(), " read_renamed_metadata");
        Ok(map)
    }

    async fn vertex_to_tree_manifest(&self, commit: &dag::Vertex) -> Result<TreeManifest> {
        let commit_id = HgId::from_slice(commit.as_ref())?;
        let commit_to_tree_id = self
            .root_tree_reader
            .read_root_tree_ids(vec![commit_id])
            .await?;
        if commit_to_tree_id.is_empty() {
            return Err(CopyTraceError::RootTreeIdNotFound(commit_id).into());
        }
        let (_, tree_id) = commit_to_tree_id[0];
        Ok(TreeManifest::durable(self.tree_store.clone(), tree_id))
    }

    async fn trace_rename_commit(
        &self,
        src: dag::Vertex,
        dst: dag::Vertex,
        path: RepoPathBuf,
    ) -> Result<Option<dag::Vertex>> {
        let set = self.dag.range(src.into(), dst.into()).await?;
        let mut rename_tracer = RenameTracer::new(
            set,
            path,
            self.root_tree_reader.clone(),
            self.tree_store.clone(),
        )
        .await?;
        let rename_commit = rename_tracer.next().await?;
        Ok(rename_commit)
    }

    async fn find_renames_in_direction(
        &self,
        commit: dag::Vertex,
        direction: SearchDirection,
    ) -> Result<(HashMap<RepoPathBuf, RepoPathBuf>, dag::Vertex)> {
        let parents = self.dag.parent_names(commit.clone()).await?;
        if parents.is_empty() {
            return Err(CopyTraceError::NoParents(commit).into());
        }
        // For simplicity, we only check p1.
        let p1 = &parents[0];
        let old_manifest = self.vertex_to_tree_manifest(p1).await?;
        let new_manifest = self.vertex_to_tree_manifest(&commit).await?;
        let renames = self.find_renames(&old_manifest, &new_manifest).await?;
        let (renames, next_commit) = match direction {
            SearchDirection::Backward => (renames, p1.clone()),
            SearchDirection::Forward => {
                let renames = renames
                    .into_iter()
                    .map(|(k, v)| (v, k))
                    .sorted()
                    .collect::<HashMap<_, _>>();
                (renames, commit)
            }
        };
        Ok((renames, next_commit))
    }

    async fn check_path(
        &self,
        commit: &dag::Vertex,
        path: RepoPathBuf,
    ) -> Result<Option<RepoPathBuf>> {
        let tree = self.vertex_to_tree_manifest(commit).await?;
        if tree.get(&path)?.is_some() {
            Ok(Some(path))
        } else {
            Ok(None)
        }
    }
}

#[async_trait]
impl CopyTrace for DagCopyTrace {
    async fn trace_rename(
        &self,
        src: dag::Vertex,
        dst: dag::Vertex,
        src_path: RepoPathBuf,
    ) -> Result<Option<RepoPathBuf>> {
        tracing::debug!(?src, ?dst, ?src_path, "trace_reanme");
        if self.dag.is_ancestor(src.clone(), dst.clone()).await? {
            return self
                .trace_rename_forward(src.clone(), dst.clone(), src_path)
                .await;
        } else if self.dag.is_ancestor(dst.clone(), src.clone()).await? {
            return self
                .trace_rename_backward(dst.clone(), src.clone(), src_path)
                .await;
        } else {
            let set = dag::Set::from_static_names(vec![src.clone(), dst.clone()]);
            let base = match self.dag.gca_one(set).await? {
                Some(base) => base,
                None => {
                    tracing::trace!("no common ancestor");
                    return Ok(None);
                }
            };
            tracing::trace!(?base);
            let base_path = self
                .trace_rename_backward(base.clone(), src, src_path)
                .await?;
            tracing::trace!(?base_path);
            if let Some(base_path) = base_path {
                return self.trace_rename_forward(base, dst, base_path).await;
            } else {
                return Ok(None);
            }
        }
    }

    async fn trace_rename_backward(
        &self,
        src: dag::Vertex,
        dst: dag::Vertex,
        dst_path: RepoPathBuf,
    ) -> Result<Option<RepoPathBuf>> {
        tracing::trace!(?src, ?dst, ?dst_path, "trace_rename_backward");
        let (mut curr, target, mut curr_path) = (dst, src, dst_path);

        loop {
            tracing::trace!(?curr, ?curr_path, " loop starts");
            let rename_commit = match self
                .trace_rename_commit(target.clone(), curr.clone(), curr_path.clone())
                .await?
            {
                Some(rename_commit) => rename_commit,
                None => return self.check_path(&target, curr_path).await,
            };
            tracing::trace!(?rename_commit, " found");

            if rename_commit == target {
                return Ok(Some(curr_path));
            }
            let (renames, next_commit) = self
                .find_renames_in_direction(rename_commit, SearchDirection::Backward)
                .await?;
            if let Some(next_path) = renames.get(&curr_path) {
                curr = next_commit;
                curr_path = next_path.clone();
            } else {
                // no rename info for curr_path
                return Ok(None);
            }
        }
    }

    async fn trace_rename_forward(
        &self,
        src: dag::Vertex,
        dst: dag::Vertex,
        src_path: RepoPathBuf,
    ) -> Result<Option<RepoPathBuf>> {
        tracing::trace!(?src, ?dst, ?src_path, "trace_rename_forward");
        let (mut curr, target, mut curr_path) = (src, dst, src_path);

        loop {
            tracing::trace!(?curr, ?curr_path, " loop starts");
            let rename_commit = match self
                .trace_rename_commit(curr.clone(), target.clone(), curr_path.clone())
                .await?
            {
                Some(rename_commit) => rename_commit,
                None => return self.check_path(&target, curr_path).await,
            };
            tracing::trace!(?rename_commit, " found");

            if rename_commit == curr {
                return Ok(Some(curr_path));
            }
            let (renames, next_commit) = self
                .find_renames_in_direction(rename_commit, SearchDirection::Forward)
                .await?;
            if let Some(next_path) = renames.get(&curr_path) {
                curr = next_commit;
                curr_path = next_path.clone();
            } else {
                // no rename info for curr_path
                return Ok(None);
            }
        }
    }

    async fn find_renames(
        &self,
        old_tree: &TreeManifest,
        new_tree: &TreeManifest,
    ) -> Result<HashMap<RepoPathBuf, RepoPathBuf>> {
        // todo:
        // * [x] parse file header and get mv info
        // * support content similarity for sl repo
        // * support content similarity for git repo
        let mut new_files = Vec::new();

        {
            // this block is for dropping matcher and diff at the end of the block,
            // otherwise the compiler compilains variable might be used across 'await'

            let matcher = AlwaysMatcher::new();
            let diff = Diff::new(old_tree, new_tree, &matcher)?;
            for entry in diff {
                let entry = entry?;

                if let DiffType::RightOnly(file_metadata) = entry.diff_type {
                    let path = entry.path;
                    let key = Key {
                        path,
                        hgid: file_metadata.hgid,
                    };
                    new_files.push(key);
                }
            }
        }

        self.read_renamed_metadata(new_files).await
    }
}

/// SearchDirection when searching renames.
///
/// Assuming we have a commit graph like below:
///
///  a..z # draw dag syntax
///
/// Forward means searching from a to z.
/// Backward means searching from z to a.
#[derive(Debug)]
enum SearchDirection {
    Forward,
    Backward,
}
