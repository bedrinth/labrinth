use super::ids::*;
use crate::database::models;
use crate::database::models::DatabaseError;
use crate::models::collections::CollectionStatus;
use chrono::{DateTime, Utc};
use redis::cmd;
use serde::{Deserialize, Serialize};

const COLLECTIONS_NAMESPACE: &str = "collections";
const DEFAULT_EXPIRY: i64 = 1800; // 30 minutes

#[derive(Clone)]
pub struct CollectionBuilder {
    pub collection_id: CollectionId,
    pub user_id: UserId,
    pub title: String,
    pub description: String,
    pub status: CollectionStatus,
    pub projects: Vec<ProjectId>,
}

impl CollectionBuilder {
    pub async fn insert(
        self,
        transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ) -> Result<CollectionId, DatabaseError> {
        let collection_struct = Collection {
            id: self.collection_id,
            title: self.title,
            user_id: self.user_id,
            description: self.description,
            created: Utc::now(),
            updated: Utc::now(),
            icon_url: None,
            color: None,
            status: self.status,
            projects: self.projects,
        };
        collection_struct.insert(&mut *transaction).await?;

        Ok(self.collection_id)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Collection {
    pub id: CollectionId,
    pub user_id: UserId,
    pub title: String,
    pub description: String,
    pub created: DateTime<Utc>,
    pub updated: DateTime<Utc>,
    pub icon_url: Option<String>,
    pub color: Option<u32>,
    pub status: CollectionStatus,
    pub projects: Vec<ProjectId>,
}

impl Collection {
    pub async fn insert(
        &self,
        transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ) -> Result<(), DatabaseError> {
        sqlx::query!(
            "
            INSERT INTO collections (
                id, user_id, title, description, 
                created, icon_url, status
            )
            VALUES (
                $1, $2, $3, $4, 
                $5, $6, $7
            )
            ",
            self.id as CollectionId,
            self.user_id as UserId,
            &self.title,
            &self.description,
            self.created,
            self.icon_url.as_ref(),
            self.status.to_string(),
        )
        .execute(&mut *transaction)
        .await?;

        for project_id in self.projects.iter() {
            sqlx::query!(
                "
                    INSERT INTO collections_mods (collection_id, mod_id)
                    VALUES ($1, $2)
                    ON CONFLICT DO NOTHING
                ",
                self.id as CollectionId,
                *project_id as ProjectId,
            )
            .execute(&mut *transaction)
            .await?;
        }

        Ok(())
    }

    pub async fn remove(
        id: CollectionId,
        transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        redis: &deadpool_redis::Pool,
    ) -> Result<Option<()>, DatabaseError> {
        let collection = Self::get(id, &mut *transaction, redis).await?;

        if let Some(collection) = collection {
            sqlx::query!(
                "
                DELETE FROM collections_mods
                WHERE collection_id = $1
                ",
                id as CollectionId,
            )
            .execute(&mut *transaction)
            .await?;

            sqlx::query!(
                "
                DELETE FROM collections
                WHERE id = $1
                ",
                id as CollectionId,
            )
            .execute(&mut *transaction)
            .await?;

            models::Collection::clear_cache(collection.id, redis).await?;

            Ok(Some(()))
        } else {
            Ok(None)
        }
    }

    pub async fn get<'a, 'b, E>(
        id: CollectionId,
        executor: E,
        redis: &deadpool_redis::Pool,
    ) -> Result<Option<Collection>, DatabaseError>
    where
        E: sqlx::Executor<'a, Database = sqlx::Postgres>,
    {
        Collection::get_many(&[id], executor, redis)
            .await
            .map(|x| x.into_iter().next())
    }

    pub async fn get_many<'a, E>(
        collection_ids: &[CollectionId],
        exec: E,
        redis: &deadpool_redis::Pool,
    ) -> Result<Vec<Collection>, DatabaseError>
    where
        E: sqlx::Executor<'a, Database = sqlx::Postgres>,
    {
        use futures::TryStreamExt;

        if collection_ids.is_empty() {
            return Ok(Vec::new());
        }

        let mut redis = redis.get().await?;

        let mut found_collections = Vec::new();
        let mut remaining_collections: Vec<CollectionId> = collection_ids.to_vec();

        if !collection_ids.is_empty() {
            let collections = cmd("MGET")
                .arg(
                    collection_ids
                        .iter()
                        .map(|x| format!("{}:{}", COLLECTIONS_NAMESPACE, x.0))
                        .collect::<Vec<_>>(),
                )
                .query_async::<_, Vec<Option<String>>>(&mut redis)
                .await?;

            for collection in collections {
                if let Some(collection) =
                    collection.and_then(|x| serde_json::from_str::<Collection>(&x).ok())
                {
                    remaining_collections.retain(|x| collection.id.0 != x.0);
                    found_collections.push(collection);
                    continue;
                }
            }
        }

        if !remaining_collections.is_empty() {
            let collection_ids_parsed: Vec<i64> =
                remaining_collections.iter().map(|x| x.0).collect();
            let db_collections: Vec<Collection> = sqlx::query!(
                "
                SELECT c.id id, c.title title, c.description description,
                c.icon_url icon_url, c.color color, c.created created, c.user_id user_id,
                c.updated updated, c.status status,
                ARRAY_AGG(DISTINCT cm.mod_id) filter (where cm.mod_id is not null) mods
                FROM collections c
                LEFT JOIN collections_mods cm ON cm.collection_id = c.id
                WHERE c.id = ANY($1)
                GROUP BY c.id;
                ",
                &collection_ids_parsed,
            )
            .fetch_many(exec)
            .try_filter_map(|e| async {
                Ok(e.right().map(|m| {
                    let id = m.id;

                    Collection {
                        id: CollectionId(id),
                        user_id: UserId(m.user_id),
                        title: m.title.clone(),
                        description: m.description.clone(),
                        icon_url: m.icon_url.clone(),
                        color: m.color.map(|x| x as u32),
                        created: m.created,
                        updated: m.updated,
                        status: CollectionStatus::from_str(&m.status),
                        projects: m
                            .mods
                            .unwrap_or_default()
                            .into_iter()
                            .map(ProjectId)
                            .collect(),
                    }
                }))
            })
            .try_collect::<Vec<Collection>>()
            .await?;

            for collection in db_collections {
                cmd("SET")
                    .arg(format!("{}:{}", COLLECTIONS_NAMESPACE, collection.id.0))
                    .arg(serde_json::to_string(&collection)?)
                    .arg("EX")
                    .arg(DEFAULT_EXPIRY)
                    .query_async::<_, ()>(&mut redis)
                    .await?;

                found_collections.push(collection);
            }
        }

        Ok(found_collections)
    }

    pub async fn clear_cache(
        id: CollectionId,
        redis: &deadpool_redis::Pool,
    ) -> Result<(), DatabaseError> {
        let mut redis = redis.get().await?;
        let mut cmd = cmd("DEL");

        cmd.arg(format!("{}:{}", COLLECTIONS_NAMESPACE, id.0));
        cmd.query_async::<_, ()>(&mut redis).await?;

        Ok(())
    }
}