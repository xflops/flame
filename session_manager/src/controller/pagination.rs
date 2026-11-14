/*
Copyright 2023 The Flame Authors.
Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at
    http://www.apache.org/licenses/LICENSE-2.0
Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
*/
use super::Controller;
use common::{apis::*, FlameError};

impl Controller {
    /// Return a bounded, filtered page without materializing the full collection.
    pub async fn page_applications(
        &self,
        paging: &PageRequest,
        filter: &ApplicationPageFilter,
    ) -> Result<Page<Application>, FlameError> {
        self.storage.page_applications(paging, filter).await
    }

    /// Return a bounded, filtered page without materializing the full collection.
    pub async fn page_sessions(
        &self,
        paging: &PageRequest,
        filter: &SessionPageFilter,
    ) -> Result<Page<Session>, FlameError> {
        self.storage.page_sessions(paging, filter).await
    }

    /// Return a bounded, filtered page without materializing the full collection.
    pub async fn page_nodes(
        &self,
        paging: &PageRequest,
        filter: &NodePageFilter,
    ) -> Result<Page<Node>, FlameError> {
        self.storage.page_nodes(paging, filter).await
    }

    /// Return a bounded, filtered page without materializing the full collection.
    pub async fn page_executors(
        &self,
        paging: &PageRequest,
        filter: &ExecutorPageFilter,
    ) -> Result<Page<Executor>, FlameError> {
        self.storage.page_executors(paging, filter).await
    }

    /// Return a bounded, filtered page without materializing the full collection.
    pub async fn page_tasks(
        &self,
        paging: &PageRequest,
        filter: &TaskPageFilter,
    ) -> Result<Page<Task>, FlameError> {
        self.storage.page_tasks(paging, filter).await
    }
}
