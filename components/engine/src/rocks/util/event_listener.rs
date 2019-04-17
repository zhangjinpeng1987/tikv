// Copyright 2017 TiKV Project Authors. Licensed under Apache-2.0.

use crate::rocks::util::engine_metrics::*;
use crate::rocks::{
    CompactionJobInfo, FlushJobInfo, IngestionInfo, WriteStallCondition, WriteStallInfo,
};

pub struct EventListener {
    db_name: String,
}

impl EventListener {
    pub fn new(db_name: &str) -> EventListener {
        EventListener {
            db_name: db_name.to_owned(),
        }
    }
}

#[inline]
fn tag_write_stall_condition(e: WriteStallCondition) -> &'static str {
    match e {
        WriteStallCondition::Normal => "normal",
        WriteStallCondition::Delayed => "delayed",
        WriteStallCondition::Stopped => "stopped",
    }
}

impl engine_rocksdb::EventListener for EventListener {
    fn on_flush_completed(&self, info: &FlushJobInfo) {
        STORE_ENGINE_EVENT_COUNTER_VEC
            .with_label_values(&[&self.db_name, info.cf_name(), "flush"])
            .inc();
        STORE_ENGINE_STALL_CONDITIONS_CHANGED_VEC
            .with_label_values(&[&self.db_name, info.cf_name(), "triggered_writes_slowdown"])
            .set(info.triggered_writes_slowdown() as i64);
        STORE_ENGINE_STALL_CONDITIONS_CHANGED_VEC
            .with_label_values(&[&self.db_name, info.cf_name(), "triggered_writes_stop"])
            .set(info.triggered_writes_stop() as i64);
    }

    fn on_compaction_completed(&self, info: &CompactionJobInfo) {
        STORE_ENGINE_EVENT_COUNTER_VEC
            .with_label_values(&[&self.db_name, info.cf_name(), "compaction"])
            .inc();
        STORE_ENGINE_COMPACTION_DURATIONS_VEC
            .with_label_values(&[&self.db_name, info.cf_name()])
            .observe(info.elapsed_micros() as f64 / 1_000_000.0);
        STORE_ENGINE_COMPACTION_NUM_CORRUPT_KEYS_VEC
            .with_label_values(&[&self.db_name, info.cf_name()])
            .inc_by(info.num_corrupt_keys() as i64);
        STORE_ENGINE_COMPACTION_REASON_VEC
            .with_label_values(&[
                &self.db_name,
                info.cf_name(),
                &info.compaction_reason().to_string(),
            ])
            .inc();
    }

    fn on_external_file_ingested(&self, info: &IngestionInfo) {
        STORE_ENGINE_EVENT_COUNTER_VEC
            .with_label_values(&[&self.db_name, info.cf_name(), "ingestion"])
            .inc();
    }

    fn on_stall_conditions_changed(&self, info: &WriteStallInfo) {
        STORE_ENGINE_EVENT_COUNTER_VEC
            .with_label_values(&[&self.db_name, info.cf_name(), "stall_conditions_changed"])
            .inc();

        STORE_ENGINE_STALL_CONDITIONS_CHANGED_VEC
            .with_label_values(&[
                &self.db_name,
                info.cf_name(),
                tag_write_stall_condition(info.cur()),
            ])
            .set(1);
        STORE_ENGINE_STALL_CONDITIONS_CHANGED_VEC
            .with_label_values(&[
                &self.db_name,
                info.cf_name(),
                tag_write_stall_condition(info.prev()),
            ])
            .set(0);
    }
}
