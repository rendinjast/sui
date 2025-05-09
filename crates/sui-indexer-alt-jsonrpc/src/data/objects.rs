// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::{collections::HashMap, sync::Arc};

use anyhow::Context as _;
use async_graphql::dataloader::Loader;
use diesel::{BoolExpressionMethods, ExpressionMethods, QueryDsl};
use serde::de::DeserializeOwned;
use sui_indexer_alt_schema::{objects::StoredObject, schema::kv_objects};
use sui_types::{base_types::ObjectID, object::Object, storage::ObjectKey};

use crate::context::Context;

use super::{
    bigtable_reader::BigtableReader, error::Error, object_info::LatestObjectInfoKey,
    object_versions::LatestObjectVersionKey, pg_reader::PgReader,
};

/// Key for fetching the contents a particular version of an object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct VersionedObjectKey(pub ObjectID, pub u64);

#[async_trait::async_trait]
impl Loader<VersionedObjectKey> for PgReader {
    type Value = StoredObject;
    type Error = Arc<Error>;

    async fn load(
        &self,
        keys: &[VersionedObjectKey],
    ) -> Result<HashMap<VersionedObjectKey, StoredObject>, Self::Error> {
        use kv_objects::dsl as o;

        if keys.is_empty() {
            return Ok(HashMap::new());
        }

        let mut conn = self.connect().await.map_err(Arc::new)?;

        let mut query = o::kv_objects.into_boxed();

        for VersionedObjectKey(id, version) in keys {
            query = query.or_filter(
                o::object_id
                    .eq(id.into_bytes())
                    .and(o::object_version.eq(*version as i64)),
            );
        }

        let objects: Vec<StoredObject> = conn.results(query).await.map_err(Arc::new)?;

        let key_to_stored: HashMap<_, _> = objects
            .iter()
            .map(|stored| {
                let id = &stored.object_id[..];
                let version = stored.object_version as u64;
                ((id, version), stored)
            })
            .collect();

        Ok(keys
            .iter()
            .filter_map(|key| {
                let slice: &[u8] = key.0.as_ref();
                let stored = *key_to_stored.get(&(slice, key.1))?;
                Some((*key, stored.clone()))
            })
            .collect())
    }
}

#[async_trait::async_trait]
impl Loader<VersionedObjectKey> for BigtableReader {
    type Value = Object;
    type Error = Arc<Error>;

    async fn load(
        &self,
        keys: &[VersionedObjectKey],
    ) -> Result<HashMap<VersionedObjectKey, Object>, Self::Error> {
        if keys.is_empty() {
            return Ok(HashMap::new());
        }

        let object_keys: Vec<ObjectKey> = keys
            .iter()
            .map(|key| ObjectKey(key.0, key.1.into()))
            .collect();

        Ok(self
            .objects(&object_keys)
            .await?
            .into_iter()
            .map(|o| (VersionedObjectKey(o.id(), o.version().into()), o))
            .collect())
    }
}

/// Load the contents of an object from the store and deserialize it as an `Object`. This function
/// does not respect deletion and wrapping. If an object is deleted or wrapped, it may return the
/// contents of the object before the deletion or wrapping, or it may return `None` if the object
/// has been fully pruned from the versions table.
pub(crate) async fn load_latest(
    ctx: &Context,
    object_id: ObjectID,
) -> Result<Option<Object>, anyhow::Error> {
    let Some(latest_version) = ctx
        .pg_loader()
        .load_one(LatestObjectVersionKey(object_id))
        .await
        .context("Failed to load latest version")?
    else {
        return Ok(None);
    };

    let object = ctx
        .kv_loader()
        .load_one_object(object_id, latest_version.object_version as u64)
        .await
        .context("Failed to load latest object")?;

    Ok(object)
}

/// Fetch the latest version of the object at ID `object_id`, and deserialize its contents as a
/// Rust type `T`, assuming that it is a Move object (not a package). This function does not
/// respect deletion and wrapping, see [load_latest] for more information.
pub(crate) async fn load_latest_deserialized<T: DeserializeOwned>(
    ctx: &Context,
    object_id: ObjectID,
) -> Result<T, anyhow::Error> {
    let object = load_latest(ctx, object_id)
        .await?
        .context("No data found")?;

    let move_object = object.data.try_as_move().context("Not a Move object")?;
    bcs::from_bytes(move_object.contents()).context("Failed to deserialize Move value")
}

/// Load the latest contents of an object from the store as long as the object is live (not deleted
/// or wrapped) and deserialize it as an `Object`.
pub(crate) async fn load_live(
    ctx: &Context,
    object_id: ObjectID,
) -> Result<Option<Object>, anyhow::Error> {
    let Some(obj_info) = ctx
        .pg_loader()
        .load_one(LatestObjectInfoKey(object_id))
        .await
        .context("Failed to fetch object info")?
    else {
        return Ok(None);
    };

    // If the latest object info record has no owner, the object is not live (it is wrapped or
    // deleted).
    if obj_info.owner_kind.is_none() {
        return Ok(None);
    }

    Ok(Some(load_latest(ctx, object_id).await?.context(
        "Failed to find content for latest version of live object",
    )?))
}
