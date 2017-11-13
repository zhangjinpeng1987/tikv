// Copyright 2017 PingCAP, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// See the License for the specific language governing permissions and
// limitations under the License.

use prometheus::{exponential_buckets, Counter, Histogram};

lazy_static! {
    pub static ref REWRITE_ENTRIES_COUNT_HISTOGRAM: Histogram =
        register_histogram!(
            "tikv_raftengine_rewrite_entries_count",
            "Bucketed histogram of rewrite entries count.",
            exponential_buckets(1.0, 2.0, 8).unwrap()
        ).unwrap();

    pub static ref REWRITE_COUNTER: Counter =
        register_counter!(
            "tikv_raftengine_rewrite_counter",
            "Total number of rewriting happens"
        ).unwrap();

    pub static ref NEED_COMPACT_REGIONS_HISTOGRAM: Histogram =
        register_histogram!(
            "tikv_raftengine_need_compact_regions_count",
            "Bucketed histogram of regions count need compact.",
            exponential_buckets(1.0, 2.0, 20).unwrap()
        ).unwrap();
}
