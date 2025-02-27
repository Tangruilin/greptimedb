// Copyright 2023 Greptime Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use common_catalog::consts::FILE_ENGINE;
use common_error::ext::BoxedError;
use common_query::Output;
use common_recordbatch::SendableRecordBatchStream;
use common_telemetry::{error, info};
use object_store::ObjectStore;
use snafu::{ensure, OptionExt};
use store_api::metadata::RegionMetadataRef;
use store_api::region_engine::RegionEngine;
use store_api::region_request::{
    RegionCloseRequest, RegionCreateRequest, RegionDropRequest, RegionOpenRequest, RegionRequest,
};
use store_api::storage::{RegionId, ScanRequest};
use tokio::sync::{Mutex, RwLock};

use crate::config::EngineConfig;
use crate::error::{
    RegionNotFoundSnafu, Result as EngineResult, UnexpectedEngineSnafu, UnsupportedSnafu,
};
use crate::region::{FileRegion, FileRegionRef};

pub struct FileRegionEngine {
    inner: EngineInnerRef,
}

impl FileRegionEngine {
    pub fn new(_config: EngineConfig, object_store: ObjectStore) -> Self {
        Self {
            inner: Arc::new(EngineInner::new(object_store)),
        }
    }
}

#[async_trait]
impl RegionEngine for FileRegionEngine {
    fn name(&self) -> &str {
        FILE_ENGINE
    }

    async fn handle_request(
        &self,
        region_id: RegionId,
        request: RegionRequest,
    ) -> Result<Output, BoxedError> {
        self.inner
            .handle_request(region_id, request)
            .await
            .map_err(BoxedError::new)
    }

    async fn handle_query(
        &self,
        region_id: RegionId,
        request: ScanRequest,
    ) -> Result<SendableRecordBatchStream, BoxedError> {
        self.inner
            .get_region(region_id)
            .await
            .context(RegionNotFoundSnafu { region_id })
            .map_err(BoxedError::new)?
            .query(request)
            .map_err(BoxedError::new)
    }

    async fn get_metadata(&self, region_id: RegionId) -> Result<RegionMetadataRef, BoxedError> {
        self.inner
            .get_region(region_id)
            .await
            .map(|r| r.metadata())
            .context(RegionNotFoundSnafu { region_id })
            .map_err(BoxedError::new)
    }

    async fn stop(&self) -> Result<(), BoxedError> {
        self.inner.stop().await.map_err(BoxedError::new)
    }

    fn set_writable(&self, region_id: RegionId, writable: bool) -> Result<(), BoxedError> {
        self.inner
            .set_writable(region_id, writable)
            .map_err(BoxedError::new)
    }
}

struct EngineInner {
    /// All regions opened by the engine.
    ///
    /// Writing to `regions` should also hold the `region_mutex`.
    regions: RwLock<HashMap<RegionId, FileRegionRef>>,

    /// Region mutex is used to protect the operations such as creating/opening/closing
    /// a region, to avoid things like opening the same region simultaneously.
    region_mutex: Mutex<()>,

    object_store: ObjectStore,
}

type EngineInnerRef = Arc<EngineInner>;

impl EngineInner {
    fn new(object_store: ObjectStore) -> Self {
        Self {
            regions: RwLock::new(HashMap::new()),
            region_mutex: Mutex::new(()),
            object_store,
        }
    }

    async fn handle_request(
        &self,
        region_id: RegionId,
        request: RegionRequest,
    ) -> EngineResult<Output> {
        match request {
            RegionRequest::Create(req) => self.handle_create(region_id, req).await,
            RegionRequest::Drop(req) => self.handle_drop(region_id, req).await,
            RegionRequest::Open(req) => self.handle_open(region_id, req).await,
            RegionRequest::Close(req) => self.handle_close(region_id, req).await,
            _ => UnsupportedSnafu {
                operation: request.to_string(),
            }
            .fail(),
        }
    }

    async fn stop(&self) -> EngineResult<()> {
        let _lock = self.region_mutex.lock().await;
        self.regions.write().await.clear();
        Ok(())
    }

    fn set_writable(&self, _region_id: RegionId, _writable: bool) -> EngineResult<()> {
        // TODO(zhongzc): Improve the semantics and implementation of this API.
        Ok(())
    }
}

impl EngineInner {
    async fn handle_create(
        &self,
        region_id: RegionId,
        request: RegionCreateRequest,
    ) -> EngineResult<Output> {
        ensure!(
            request.engine == FILE_ENGINE,
            UnexpectedEngineSnafu {
                engine: request.engine
            }
        );

        if self.exists(region_id).await {
            return Ok(Output::AffectedRows(0));
        }

        info!("Try to create region, region_id: {}", region_id);

        let _lock = self.region_mutex.lock().await;
        // Check again after acquiring the lock
        if self.exists(region_id).await {
            return Ok(Output::AffectedRows(0));
        }

        let res = FileRegion::create(region_id, request, &self.object_store).await;
        let region = res.inspect_err(|err| {
            error!(
                "Failed to create region, region_id: {}, err: {}",
                region_id, err
            );
        })?;
        self.regions.write().await.insert(region_id, region);

        info!("A new region is created, region_id: {}", region_id);
        Ok(Output::AffectedRows(0))
    }

    async fn handle_open(
        &self,
        region_id: RegionId,
        request: RegionOpenRequest,
    ) -> EngineResult<Output> {
        if self.exists(region_id).await {
            return Ok(Output::AffectedRows(0));
        }

        info!("Try to open region, region_id: {}", region_id);

        let _lock = self.region_mutex.lock().await;
        // Check again after acquiring the lock
        if self.exists(region_id).await {
            return Ok(Output::AffectedRows(0));
        }

        let res = FileRegion::open(region_id, request, &self.object_store).await;
        let region = res.inspect_err(|err| {
            error!(
                "Failed to open region, region_id: {}, err: {}",
                region_id, err
            );
        })?;
        self.regions.write().await.insert(region_id, region);

        info!("Region opened, region_id: {}", region_id);
        Ok(Output::AffectedRows(0))
    }

    async fn handle_close(
        &self,
        region_id: RegionId,
        _request: RegionCloseRequest,
    ) -> EngineResult<Output> {
        let _lock = self.region_mutex.lock().await;

        let mut regions = self.regions.write().await;
        if regions.remove(&region_id).is_some() {
            info!("Region closed, region_id: {}", region_id);
        }

        Ok(Output::AffectedRows(0))
    }

    async fn handle_drop(
        &self,
        region_id: RegionId,
        _request: RegionDropRequest,
    ) -> EngineResult<Output> {
        if !self.exists(region_id).await {
            return RegionNotFoundSnafu { region_id }.fail();
        }

        info!("Try to drop region, region_id: {}", region_id);

        let _lock = self.region_mutex.lock().await;

        let region = self.get_region(region_id).await;
        if let Some(region) = region {
            let res = FileRegion::drop(&region, &self.object_store).await;
            res.inspect_err(|err| {
                error!(
                    "Failed to drop region, region_id: {}, err: {}",
                    region_id, err
                );
            })?;
        }
        let _ = self.regions.write().await.remove(&region_id);

        info!("Region dropped, region_id: {}", region_id);
        Ok(Output::AffectedRows(0))
    }

    async fn get_region(&self, region_id: RegionId) -> Option<FileRegionRef> {
        self.regions.read().await.get(&region_id).cloned()
    }

    async fn exists(&self, region_id: RegionId) -> bool {
        self.regions.read().await.contains_key(&region_id)
    }
}
