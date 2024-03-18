//! # Versioning helpers and utilities
//!
//! This module provides access to versioning tools.
//!
//! # Traits
//!
//! The module exposes three main traits that can be implemented to provide versioning logic:
//!
//! * `VersionedRow`: Gives this module access to versioning attributes such as valid_to. Implement
//!   this trait to enable setting these attributes automatically to use batch insertion.
//!
//! * `DeltaVersionedRow`: Same as above but will also set `previous_value`` attributes.
//!
//! * `StoredVersionedRow`: Enables setting the end version on currently active version in the db
//!   based on new incoming entries.
//!
//! ## Notes
//! To use the apply_versioning function defined here VersionRow::EntityId and
//! StoredVersionedRow::EntityId must be of the same type. Keep that in mind while implementing
//! these traits on your structs.
//!
//! # Design Decisions
//!
//! Initially we would support references in EntityId, to reduce the number of clones necessary for
//! complex entity id types. This would lead to a strange situation, where these trait bounds
//! for the `apply_versioning` method would not be expressible. Reasons for this are not 100% clear,
//! however, `latest_versions_by_ids` referring to the `StoredVersionedRow::EntityId`` but actually
//! being used with `VersionedRow::EntityId` is most likely related. Previous iterations had
//! lifetimes on `StoredVersionedRow<'a>` but as said, the management of lifetimes became
//! increasingly complex to a point where apply_versioning was not always usable.
//!
//! Instead we removed support for references in the EntityId type for now and just accept the high
//! number of clones necessary. This may be revisited later again in case the clones become a
//! performance issue.
//! There are basically two versions to resolve this, modify the ORM structs to use smart pointers
//! thus making the clones cheap. Or modify the traits and the function defined here to work around
//! the lifetime issues.
use crate::postgres::PostgresError;
use async_trait::async_trait;
use chrono::NaiveDateTime;
use diesel::{
    pg::Pg,
    query_builder::{BoxedSqlQuery, SqlQuery},
    sql_query,
    sql_types::{BigInt, Timestamp},
};
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use itertools::Itertools;
use std::{collections::HashMap, fmt::Debug, hash::Hash};
use tycho_core::storage::StorageError;

/// Trait indicating that a struct can be inserted into a versioned table.
///
/// This trait enables querying the struct for its current state and allows to set the `valid_to``
/// column in case we are inserting a historical row (row that is outdated at the time of insertion,
/// but contributes to the history of the entity).
pub trait VersionedRow {
    /// Rust type to use as key to sort a collection of structs by entity and time.
    type SortKey: Ord + Clone + Debug + Send + Sync;
    /// The entity identifier type.
    type EntityId: Ord + Hash + Debug + Send + Sync;
    /// The version type.
    type Version: Ord + Copy + Debug + Send + Sync;

    /// Exposes the entity identifier.
    fn get_entity_id(&self) -> Self::EntityId;

    /// Allows setting `valid_to`` column, thereby invalidating this version.
    fn set_valid_to(&mut self, end_version: Self::Version);

    /// Exposes the starting version.
    fn get_valid_from(&self) -> Self::Version;
}

/// Trait indicating that a struct can be inserted in a delta versioned table.
///
/// Delta versioned records require the previous value present in one of their columns. This serves
/// to build both forward and backward delta changes while avoiding self joins.
pub trait DeltaVersionedRow {
    type Value: Clone + Debug;

    /// Exposes the current value.
    fn get_value(&self) -> Self::Value;

    /// Sets the previous value.
    fn set_previous_value(&mut self, previous_value: Self::Value);
}

/// Trait indicating that a struct relates to a stored entry in a versioned table.
///
/// This struct is used to invalidate rows that are currently valid on the db side before inserting
/// new versions for those entities.
///
/// ## Note
/// The associated types of this trait need to match with the types defined for the corresponding
/// `VersionedRow` trait.
#[async_trait]
pub trait StoredVersionedRow {
    /// The entity identifier type.
    type EntityId: Ord + Hash + Debug + Send + Sync;
    /// The primary key on the table for this row.
    type PrimaryKey: Into<i64> + Debug + Send + Sync;
    /// The version type.
    type Version: Into<NaiveDateTime> + Copy + Debug + Send + Sync;

    /// Exposes the primary key.
    fn get_pk(&self) -> Self::PrimaryKey;

    /// Exposes the entity id.
    fn get_entity_id(&self) -> Self::EntityId;

    /// Retrieves the latest versions for the passed entity ids from the database.
    async fn latest_versions_by_ids<I: IntoIterator<Item = Self::EntityId> + Send + Sync>(
        ids: I,
        conn: &mut AsyncPgConnection,
    ) -> Result<Vec<Box<Self>>, StorageError>;

    /// Exposes the associated table name.
    fn table_name() -> &'static str;
}

/// Sets end versions on a collection of new rows.
///
/// This function will mutate the entries in the passed vector. It will assign a end
/// version to each row if there is a duplicated entity in the collection. Entities are invalidated
/// according to their sort key in ascending order.
fn set_versioning_attributes<O: VersionedRow>(
    objects: &mut [O],
) -> HashMap<O::EntityId, O::Version> {
    let mut db_updates = HashMap::new();

    db_updates.insert(objects[0].get_entity_id(), objects[0].get_valid_from());

    for i in 0..objects.len() - 1 {
        let (head, tail) = objects.split_at_mut(i + 1);
        let current = &mut head[head.len() - 1];
        let next = &tail[0];

        if current.get_entity_id() == next.get_entity_id() {
            current.set_valid_to(next.get_valid_from());
        } else {
            db_updates.insert(next.get_entity_id(), next.get_valid_from());
        }
    }
    db_updates
}

/// Sets end versions and previous values on a collection of new rows.
///
/// Same as `set_versioning_attributes` but will also set previous value for delta versioned table
/// entries.
fn set_delta_versioning_attributes<O: VersionedRow + DeltaVersionedRow + Debug>(
    objects: &mut [O],
) -> HashMap<O::EntityId, O::Version> {
    let mut db_updates = HashMap::new();

    db_updates.insert(objects[0].get_entity_id(), objects[0].get_valid_from());

    for i in 0..objects.len() - 1 {
        let (head, tail) = objects.split_at_mut(i + 1);
        let current = &mut head[head.len() - 1];
        let next = &mut tail[0];

        if current.get_entity_id() == next.get_entity_id() {
            current.set_valid_to(next.get_valid_from());
            next.set_previous_value(current.get_value())
        } else {
            db_updates.insert(next.get_entity_id(), next.get_valid_from());
        }
    }
    db_updates
}

/// Builds a update query that updates multiple rows at once.
///
/// Builds a query that will take update multiple rows end versions. The rows are identified by
/// their primary key and the version is retrieved from the `end_versions` parameter.
///
/// Building such a query with pure diesel is currently not supported as this query updates each
/// primary key with a unique value. See: https://github.com/diesel-rs/diesel/discussions/2879
fn build_batch_update_query<'a, O: StoredVersionedRow>(
    objects: &'a [Box<O>],
    table_name: &str,
    end_versions: &'a HashMap<O::EntityId, O::Version>,
) -> BoxedSqlQuery<'a, Pg, SqlQuery> {
    // Generate bind parameter 2-tuples the result will look like '($1, $2), ($3, $4), ...'
    // These are later subsituted with the primary key and valid to values.
    let bind_params = (1..=objects.len() * 2)
        .map(|i| if i % 2 == 0 { format!("${}", i) } else { format!("(${}", i) })
        .collect::<Vec<String>>()
        .chunks(2)
        .map(|chunk| chunk.join(", ") + ")")
        .collect::<Vec<String>>()
        .join(", ");
    let query_str = format!(
        r#"
        UPDATE {} as t set
            valid_to = m.valid_to
        FROM (
            VALUES {}
        ) as m(id, valid_to) 
        WHERE t.id = m.id;
        "#,
        table_name, bind_params
    );
    let mut query = sql_query(query_str).into_boxed();
    for o in objects.iter() {
        let valid_to = *end_versions
            .get(&o.get_entity_id())
            .expect("versions present for all rows");
        query = query
            .bind::<BigInt, _>(o.get_pk().into())
            .bind::<Timestamp, _>(valid_to.into());
    }
    query
}

/// Applies and execute versioning logic for a set of new entries.
///
/// This function will execute the following steps:
/// - Set end versions on a collection of new entries
/// - Given the new entries query the table currently valid versions
/// - Execute and update query to invalidate the previously retrieved entries
///
/// ## Important note:
/// This function requires that new_data is sorted by ascending execution order (block, transaction
/// index) for conflicting entity_id.
pub async fn apply_versioning<'a, N, S>(
    new_data: &mut [N],
    conn: &mut AsyncPgConnection,
) -> Result<(), StorageError>
where
    N: VersionedRow,
    S: StoredVersionedRow<EntityId = N::EntityId, Version = N::Version>,
    <N as VersionedRow>::EntityId: Clone,
{
    if new_data.is_empty() {
        return Ok(());
    }

    let end_versions = set_versioning_attributes(new_data);
    let db_rows = S::latest_versions_by_ids(end_versions.keys().cloned(), conn)
        .await
        .map_err(PostgresError::from)?;
    if !db_rows.is_empty() {
        build_batch_update_query(&db_rows, S::table_name(), &end_versions)
            .execute(conn)
            .await
            .map_err(PostgresError::from)?;
    }
    Ok(())
}

#[async_trait]
pub trait StoredDeltaVersionedRow: StoredVersionedRow {
    type Value: Clone + Debug;

    fn get_value(&self) -> Self::Value;
}

/// Applies and executes delta versioning logic for a set of new entries.
///
/// Same as `apply_versioning` but also takes care of previous value columns.
///
/// ## Important note:
/// This function requires that new_data is sorted by ascending execution order (block, transaction
/// index) for conflicting entity_id.
pub async fn apply_delta_versioning<'a, N, S>(
    new_data: &mut [N],
    conn: &mut AsyncPgConnection,
) -> Result<(), StorageError>
where
    N: VersionedRow + DeltaVersionedRow + Debug,
    S: StoredDeltaVersionedRow<EntityId = N::EntityId, Version = N::Version, Value = N::Value>,
    <N as VersionedRow>::EntityId: Clone,
{
    if new_data.is_empty() {
        return Ok(());
    }
    let end_versions = set_delta_versioning_attributes(new_data);
    let db_rows = S::latest_versions_by_ids(end_versions.keys().cloned(), conn)
        .await
        .map_err(PostgresError::from)?;

    // Not terribly efficient but works (using find is very inefficient especially if new data is
    // big)
    for r in db_rows.iter() {
        let current_id = r.get_entity_id();
        // find the first new entry with this id, we assume new_data is correctly sorted.
        if let Some(new_entry) = new_data
            .iter_mut()
            .find(|new| new.get_entity_id() == current_id)
        {
            // set this new entries previous value
            new_entry.set_previous_value(r.get_value());
        }
    }

    if !db_rows.is_empty() {
        build_batch_update_query(&db_rows, S::table_name(), &end_versions)
            .execute(conn)
            .await
            .map_err(PostgresError::from)?;
    }
    Ok(())
}

pub trait PartitionedVersionedRow: Clone + Send + Sync {
    type EntityId: Clone + Ord + Hash + Debug + Send + Sync;
    fn get_id(&self) -> Self::EntityId;
    fn get_valid_to(&self) -> NaiveDateTime;
    fn archive(&mut self, next_version: &Self);
    fn delete(&mut self, delete_version: NaiveDateTime);
    async fn latest_versions_by_ids<I: IntoIterator<Item = Self::EntityId> + Send + Sync>(
        ids: I,
        conn: &mut AsyncPgConnection,
    ) -> Result<Vec<Self>, StorageError>
    where
        Self: Sized;
}

fn set_partitioned_versioning_attributes<N: PartitionedVersionedRow>(
    data: &[N],
    delete_version: &HashMap<N::EntityId, NaiveDateTime>,
) -> (Vec<N>, Vec<N>) {
    let mut latest = HashMap::<N::EntityId, N>::new();
    let mut archived = Vec::new();
    for i in 0..data.len() {
        let id = data[i].get_id();

        // Handle deleted rows
        if let Some(delete_version) = delete_version.get(&id) {
            let mut delete_row = data[i].clone();
            delete_row.delete(*delete_version);
            archived.push(delete_row);
            continue;
        }

        // Handle updated rows
        if let Some(mut prev) = latest.remove(&id) {
            prev.archive(&data[i]);
            archived.push(prev);
        }
        latest.insert(id, data[i].clone());
    }
    (latest.into_values().collect(), archived)
}

/// Applies versioning using partitioned tables.
///
/// Applying versioning on a partitioned table is a bit more involved since we can't
/// simply update a column value that is part of the partitioning logic.
///
/// Partitioned tables are partitioned over the `valid_to` column. This means there is a table for
/// each day. Currently valid rows, are put into a default partition, since their valid_to value is
/// infinite (usually modeled with a very far in the future date).
///
/// To update a row, we to move it into an archive partition by setting its valid_to column
/// correctly. Since rows are not automatically moved between partitions upon updates, we need to
/// retrieve the row, update its `valid_to` value and insert it into the partitioned table again
/// (the routing to which exact partition is then handled by postgres automatically). Next we need
/// to update the attributes of the current version in the default partition.
///
/// In case of inserts, we can skip the archival insert since there is no previous version. The
/// update of the current state should be replaced with simple insert.
///
/// ## Batch Updates
/// If inserting a lot of rows, as is usually the case, and the update contains multiple version of
/// the same entity, we directly create the archival version on the application side saving us
/// multiple round trips to the database. This method will handle this for you.
///
/// ## Retention Horizon
/// Partitioned tables usually have a retention horizon meaning any outdated versions
/// older than the horizon are not kept in storage. To achieve this, archive versions strictly older
/// than the horizon are simply dropped before issuing the inserts.
///
/// ## Deletions
/// Deletion simply move the row from the default partition to an archive parition by setting the
/// valid_to column and skipping the update or insert into the current state.
///
/// ## Overview
///
/// This function will execute the following steps:
///
/// - Retrieve the current state of all entities to be updated or deleted.
/// - Apply application side versioning, calling either delete or archive on the respective rows.
/// - Filter any archived rows by the retention horizon.
///
/// ## Returns
/// The method returns a vector with the latest version as well as vector of archive versions.
/// The latest version are supposed to be executed as upserts into the default partition directly,
/// the archive version can simply be inserted into the partitioned table. Actually executing these
/// operations is left to the caller since the exact implementation may vary based on the table
/// schema.
///
/// ## Note
/// This method may only works for rows that have a primary key know before insert. So e.g.
/// `BIGSERIAL` primary keys won't work here since the method can only deal with a single type, so
/// you can't use a `New*` orm models here combined with an already stored orm model type.
pub async fn apply_partitioned_versioning<T: PartitionedVersionedRow>(
    new_data: &[T],
    delete_versions: &HashMap<T::EntityId, NaiveDateTime>,
    retention_horizon: NaiveDateTime,
    conn: &mut AsyncPgConnection,
) -> Result<(Vec<T>, Vec<T>), StorageError> {
    if new_data.is_empty() && delete_versions.is_empty() {
        return Ok((Vec::new(), Vec::new()))
    }
    let db_rows: Vec<T> = T::latest_versions_by_ids(
        new_data
            .iter()
            .map(|e| e.get_id())
            .chain(delete_versions.keys().cloned())
            .unique(),
        conn,
    )
    .await?
    .into_iter()
    .chain(new_data.iter().cloned())
    .collect();

    let (latest, archive) = set_partitioned_versioning_attributes(&db_rows, delete_versions);
    let filtered_archive: Vec<_> = archive
        .into_iter()
        .filter(|e| e.get_valid_to() > retention_horizon)
        .collect();
    Ok((latest, filtered_archive))
}
