use tracing::warn;

use super::{
    get_many_messages_as_bytes, get_message, make_fid_key, make_message_primary_key, make_user_key,
    store::{Store, StoreDef},
    MessagesPage, StoreEventHandler, PAGE_SIZE_MAX, TS_HASH_LENGTH,
};
use crate::{
    core::error::HubError,
    proto::{link_body::Target, SignatureScheme},
    storage::util::vec_to_u8_24,
};
use crate::{proto::message_data::Body, storage::db::PageOptions};
use crate::{proto::LinkBody, storage::util::increment_vec_u8};
use crate::{
    proto::MessageData,
    storage::constants::{RootPrefix, UserPostfix},
};
use crate::{
    proto::{Message, MessageType},
    storage::db::{RocksDB, RocksDbTransactionBatch},
};
use std::{borrow::Borrow, convert::TryInto, sync::Arc};

/**
 * LinkStore persists Link Messages in RocksDB using a two-phase CRDT set to guarantee
 * eventual consistency.
 *
 * A Link is created by a user and points at a target (e.g. fid) and has a type (e.g. "follow").
 * Links are added with a LinkAdd and removed with a LinkRemove. Link messages can
 * collide if two messages have the same user fid, target, and type. Collisions are handled with
 * Last-Write-Wins + Remove-Wins rules as follows:
 *
 * 1. Highest timestamp wins
 * 2. Remove wins over Adds
 * 3. Highest lexicographic hash wins
 *
 * LinkMessages are stored ordinally in RocksDB indexed by a unique key `fid:tsHash`,
 * which makes truncating a user's earliest messages easy. Indices are built to look up
 * link adds in the adds set, link removes in the remove set and all links
 * for a given target. The key-value entries created by the Link Store are:
 *
 * 1. fid:tsHash -> link message
 * 2. fid:set:targetCastTsHash:linkType -> fid:tsHash (Set Index)
 * 3. linkTarget:linkType:targetCastTsHash -> fid:tsHash (Target Index)
 */
#[derive(Clone)]
pub struct LinkStore {
    prune_size_limit: u32,
}

impl LinkStore {
    // Even though fid is 64 bits, we're only using 32 bits for now, to save 4 bytes per key.
    // This is fine until 4 billion users, after which we'll need to do a migration of this key in the DB.
    const FID_BYTE_SIZE: usize = 4;
    const LINK_TYPE_BYTE_SIZE: usize = 8;
    const POSTFIX_BYTE_SIZE: usize = 1;
    const ROOT_PREFIX_BYTE_SIZE: usize = 1;
    const ROOT_PREFIXED_FID_BYTE_SIZE: usize = 33;
    const TARGET_ID_BYTE_SIZE: usize = 4;

    pub fn new(
        db: Arc<RocksDB>,
        store_event_handler: Arc<StoreEventHandler>,
        prune_size_limit: u32,
    ) -> Store<LinkStore> {
        Store::new_with_store_def(db, store_event_handler, LinkStore { prune_size_limit })
    }

    /// Finds a LinkAdd Message by checking the Adds Set index.
    /// Return the LinkAdd Model if it exists, none otherwise
    ///
    /// # Arguments
    /// * `store` - the Rust data store used to query for finding a LinkAdd message
    /// * `fid` - fid of the user who created the link add
    /// * `r#type` - type of link that was added
    /// * `target` - id of the fid being linked to
    pub fn get_link_add(
        store: &Store<LinkStore>,
        fid: u32,
        r#type: String,
        target: Option<Target>,
    ) -> Result<Option<Message>, HubError> {
        let partial_message = Message {
            data: Some(MessageData {
                fid: fid as u64,
                r#type: MessageType::LinkAdd.into(),
                body: Some(Body::LinkBody(LinkBody {
                    r#type,
                    target,
                    ..Default::default()
                })),
                ..Default::default()
            }),
            ..Default::default()
        };

        let result = store.get_add(&partial_message);
        if let Ok(None) = result {
            // Check for incorrectly padded keys
            let unpadded_key = Self::make_add_key_padded(&partial_message, false)?;
            let message_ts_hash = store.db().get(&unpadded_key)?;
            if message_ts_hash.is_none() {
                return Ok(None);
            }
            return get_message(
                store.db().borrow(),
                partial_message.data.as_ref().unwrap().fid as u32,
                store.store_def().postfix(),
                &vec_to_u8_24(&message_ts_hash)?,
            );
        }
        result
    }

    pub fn get_link_adds_by_fid(
        store: &Store<LinkStore>,
        fid: u32,
        r#type: String,
        page_options: &PageOptions,
    ) -> Result<MessagesPage, HubError> {
        store.get_adds_by_fid(
            fid,
            page_options,
            Some(|message: &Message| {
                message
                    .data
                    .as_ref()
                    .is_some_and(|data| match data.body.as_ref() {
                        Some(Body::LinkBody(body)) => r#type.is_empty() || body.r#type == r#type,
                        _ => false,
                    })
            }),
        )
    }

    pub fn get_link_compact_state_message_by_fid(
        store: &Store<LinkStore>,
        fid: u32,
        page_options: &PageOptions,
    ) -> Result<MessagesPage, HubError> {
        store.get_compact_state_messages_by_fid(fid, page_options)
    }

    pub fn get_links_by_target(
        store: &Store<LinkStore>,
        target: &Target,
        r#type: String,
        page_options: &PageOptions,
    ) -> Result<MessagesPage, HubError> {
        let start_prefix: Vec<u8> = LinkStore::links_by_target_key(target, 0, None)?;

        let mut message_keys = vec![];
        let mut last_key = vec![];

        store.db().for_each_iterator_by_prefix(
            Some(start_prefix.to_vec()),
            Some(increment_vec_u8(&start_prefix)),
            page_options,
            |key, value| {
                if r#type.is_empty() || value.eq(r#type.as_bytes()) {
                    let ts_hash_offset = start_prefix.len();
                    let fid_offset: usize = ts_hash_offset + TS_HASH_LENGTH;

                    let fid =
                        u32::from_be_bytes(key[fid_offset..fid_offset + 4].try_into().unwrap());
                    let ts_hash = key[ts_hash_offset..ts_hash_offset + TS_HASH_LENGTH]
                        .try_into()
                        .unwrap();
                    let message_primary_key =
                        make_message_primary_key(fid, store.postfix(), Some(&ts_hash));

                    message_keys.push(message_primary_key.to_vec());
                    if message_keys.len() >= page_options.page_size.unwrap_or(PAGE_SIZE_MAX) {
                        last_key = key.to_vec();
                        return Ok(true); // Stop iterating
                    }
                }

                Ok(false)
            },
        )?;

        let messages_bytes = get_many_messages_as_bytes(store.db().borrow(), message_keys)?;
        let next_page_token = if last_key.len() > 0 {
            Some(last_key.to_vec())
        } else {
            None
        };

        Ok(MessagesPage {
            messages_bytes,
            next_page_token,
        })
    }

    /// Finds a LinkRemove Message by checking the Remove Set index.
    /// Return the LinkRemove message if it exists, none otherwise
    ///
    /// # Arguments
    /// * `store` - the Rust data store used to query for finding a LinkAdd message
    /// * `fid` - fid of the user who created the link add
    /// * `r#type` - type of link that was added
    /// * `target` - id of the fid being linked to
    pub fn get_link_remove(
        store: &Store<LinkStore>,
        fid: u32,
        r#type: String,
        target: Option<Target>,
    ) -> Result<Option<Message>, HubError> {
        let partial_message = Message {
            data: Some(MessageData {
                fid: fid as u64,
                r#type: MessageType::LinkRemove.into(),
                body: Some(Body::LinkBody(LinkBody {
                    r#type,
                    target,
                    ..Default::default()
                })),
                ..Default::default()
            }),
            ..Default::default()
        };

        let result = store.get_remove(&partial_message);
        if let Ok(None) = result {
            // Check for incorrectly padded keys
            let unpadded_key = Self::make_remove_key_padded(&partial_message, false)?;
            let message_ts_hash = store.db().get(&unpadded_key)?;
            if message_ts_hash.is_none() {
                return Ok(None);
            }
            return get_message(
                store.db().borrow(),
                partial_message.data.as_ref().unwrap().fid as u32,
                store.store_def().postfix(),
                &vec_to_u8_24(&message_ts_hash)?,
            );
        }
        result
    }

    // Generates a unique key used to store a LinkCompactState message key in the store
    fn link_compact_state_add_key(fid: u32, link_type: &String) -> Result<Vec<u8>, HubError> {
        let mut key = Vec::with_capacity(
            Self::ROOT_PREFIXED_FID_BYTE_SIZE + Self::POSTFIX_BYTE_SIZE + Self::LINK_TYPE_BYTE_SIZE,
        );

        key.extend_from_slice(&make_user_key(fid));
        key.push(UserPostfix::LinkCompactStateMessage.as_u8());
        let type_bytes = &mut link_type.as_bytes().to_vec();
        // Pad with zero bytes
        type_bytes.resize(Self::LINK_TYPE_BYTE_SIZE, 0);
        key.extend_from_slice(&type_bytes);

        Ok(key)
    }

    /// Generates a unique key used to store a LinkAdd message key in the LinksAdd Set index.
    /// Returns RocksDB key of the form <RootPrefix>:<fid>:<UserPostfix>:<targetKey?>:<type?>
    ///
    /// # Arguments
    /// * `fid` - farcaster id of the user who created the link
    /// * `link_body` - body of link that contains type of link created and target ID of the object
    ///                 being reacted to
    fn link_add_key(fid: u32, link_body: &LinkBody, padded: bool) -> Result<Vec<u8>, HubError> {
        if link_body.target.is_some()
            && (link_body.r#type.is_empty() || link_body.r#type.len() == 0)
        {
            return Err(HubError::validation_failure(
                "targetId provided without type",
            ));
        }

        if !link_body.r#type.is_empty()
            && (link_body.r#type.len() > Self::LINK_TYPE_BYTE_SIZE || link_body.r#type.len() == 0)
        {
            return Err(HubError::validation_failure(
                "link type invalid - non-empty link type found with invalid length",
            ));
        }

        let mut key = Vec::with_capacity(
            Self::ROOT_PREFIXED_FID_BYTE_SIZE
                + Self::POSTFIX_BYTE_SIZE
                + Self::LINK_TYPE_BYTE_SIZE
                + Self::TARGET_ID_BYTE_SIZE,
        );

        key.extend_from_slice(&make_user_key(fid));
        key.push(UserPostfix::LinkAdds.as_u8());
        let type_bytes = &mut link_body.r#type.as_bytes().to_vec();
        if padded {
            // Pad with zero bytes
            type_bytes.resize(Self::LINK_TYPE_BYTE_SIZE, 0);
        }
        key.extend_from_slice(&type_bytes);
        match link_body.target {
            None => {}
            Some(Target::TargetFid(fid)) => {
                key.extend_from_slice(&make_fid_key(fid as u32)[..Self::TARGET_ID_BYTE_SIZE])
            }
        }

        Ok(key)
    }

    /// Generates a unique key used to store a LinkRemove message key in the LinksRemove Set index.
    /// Returns RocksDB key of the form <RootPrefix>:<fid>:<UserPostfix>:<targetKey?>:<type?>
    ///
    /// # Arguments
    /// * `fid` - farcaster id of the user who created the link
    /// * `link_body` - body of link that contains type of link created and target ID of the object
    ///                 being reacted to
    fn link_remove_key(fid: u32, link_body: &LinkBody, padded: bool) -> Result<Vec<u8>, HubError> {
        if link_body.target.is_some()
            && (link_body.r#type.is_empty() || link_body.r#type.len() == 0)
        {
            return Err(HubError::validation_failure(
                "targetID provided without type",
            ));
        }

        if !link_body.r#type.is_empty()
            && (link_body.r#type.len() > Self::LINK_TYPE_BYTE_SIZE || link_body.r#type.len() == 0)
        {
            return Err(HubError::validation_failure(
                "link type invalid - non-empty link type found with invalid length",
            ));
        }

        let mut key = Vec::with_capacity(
            Self::ROOT_PREFIXED_FID_BYTE_SIZE
                + Self::POSTFIX_BYTE_SIZE
                + Self::LINK_TYPE_BYTE_SIZE
                + Self::TARGET_ID_BYTE_SIZE,
        );

        // TODO: does the fid and rtype need to be padded? Is it okay not the check their lengths?
        key.extend_from_slice(&make_user_key(fid));
        key.push(UserPostfix::LinkRemoves.as_u8());
        let type_bytes = &mut link_body.r#type.as_bytes().to_vec();
        if padded {
            // Pad with zero bytes
            type_bytes.resize(Self::LINK_TYPE_BYTE_SIZE, 0);
        }
        key.extend_from_slice(&type_bytes);
        match link_body.target {
            None => {}
            Some(Target::TargetFid(fid)) => {
                key.extend_from_slice(&make_fid_key(fid as u32)[..Self::TARGET_ID_BYTE_SIZE])
            }
        }

        Ok(key)
    }

    pub fn make_add_key_padded(message: &Message, padded: bool) -> Result<Vec<u8>, HubError> {
        message
            .data
            .as_ref()
            .ok_or(HubError::invalid_parameter("invalid message data"))
            .and_then(|data| {
                data.body
                    .as_ref()
                    .ok_or(HubError::invalid_parameter("invalid message data body"))
                    .and_then(|body_option| match body_option {
                        Body::LinkBody(link_body) => {
                            Self::link_add_key(data.fid as u32, link_body, padded)
                        }
                        _ => Err(HubError::invalid_parameter("link body not specified")),
                    })
            })
    }

    pub fn make_remove_key_padded(message: &Message, padded: bool) -> Result<Vec<u8>, HubError> {
        message
            .data
            .as_ref()
            .ok_or(HubError::invalid_parameter("invalid message data"))
            .and_then(|data| {
                data.body
                    .as_ref()
                    .ok_or(HubError::invalid_parameter("invalid message data body"))
                    .and_then(|body_option| match body_option {
                        Body::LinkBody(link_body) => {
                            Self::link_remove_key(data.fid as u32, link_body, padded)
                        }
                        _ => Err(HubError::invalid_parameter("link body not specified")),
                    })
            })
    }

    /// Generates a unique key used to store a LinkAdd Message in the LinksByTargetAndType index.
    /// Returns RocksDB index key of the form <RootPrefix>:<target_key>:<fid?>:<tsHash?>
    ///
    /// # Arguments
    /// * `target` - target ID of the object being reacted to (currently just cast id)
    /// * `fid` - the fid of the user who created the link
    /// * `ts_hash` - the timestamp hash of the link message
    fn links_by_target_key(
        target: &Target,
        fid: u32,
        ts_hash: Option<&[u8; TS_HASH_LENGTH]>,
    ) -> Result<Vec<u8>, HubError> {
        if fid != 0 && (ts_hash.is_none() || ts_hash.is_some_and(|tsh| tsh.len() == 0)) {
            return Err(HubError::validation_failure(
                "fid provided without timestamp hash",
            ));
        }

        if ts_hash.is_some() && fid == 0 {
            return Err(HubError::validation_failure(
                "timestamp hash provided without fid",
            ));
        }

        let mut key = Vec::with_capacity(
            Self::ROOT_PREFIX_BYTE_SIZE
                + Self::TARGET_ID_BYTE_SIZE
                + TS_HASH_LENGTH
                + Self::FID_BYTE_SIZE,
        );

        key.push(RootPrefix::LinksByTarget as u8);
        let Target::TargetFid(target_fid) = target;
        key.extend(make_fid_key(*target_fid as u32));

        match ts_hash {
            Some(timestamp_hash) => {
                key.extend_from_slice(timestamp_hash);
            }
            _ => {}
        }

        if fid > 0 {
            key.extend(make_fid_key(fid));
        }

        Ok(key)
    }

    fn secondary_index_key(
        &self,
        ts_hash: &[u8; TS_HASH_LENGTH],
        message: &Message,
    ) -> Result<(Vec<u8>, Vec<u8>), HubError> {
        message
            .data
            .as_ref()
            .ok_or(HubError::invalid_parameter("invalid message data"))
            .and_then(|data| {
                data.body
                    .as_ref()
                    .ok_or(HubError::invalid_parameter("invalid message data body"))
                    .and_then(|body| match body {
                        Body::LinkBody(link_body) => {
                            return link_body
                                .target
                                .as_ref()
                                .ok_or(HubError::invalid_parameter("target ID not specified"))
                                .and_then(|target| {
                                    LinkStore::links_by_target_key(
                                        target,
                                        data.fid as u32,
                                        Some(ts_hash),
                                    )
                                    .and_then(|target_key| {
                                        Ok((target_key, link_body.r#type.as_bytes().to_vec()))
                                    })
                                });
                        }
                        _ => Err(HubError::invalid_parameter("link body not specified")),
                    })
            })
    }

    pub fn get_link_removes_by_fid(
        store: &Store<LinkStore>,
        fid: u32,
        r#type: String,
        page_options: &PageOptions,
    ) -> Result<MessagesPage, HubError> {
        store.get_removes_by_fid(
            fid,
            page_options,
            Some(|message: &Message| {
                message
                    .data
                    .as_ref()
                    .is_some_and(|data| match data.body.as_ref() {
                        Some(Body::LinkBody(body)) => r#type.is_empty() || body.r#type == r#type,
                        _ => false,
                    })
            }),
        )
    }
}

impl StoreDef for LinkStore {
    fn postfix(&self) -> u8 {
        UserPostfix::LinkMessage.as_u8()
    }

    fn add_message_type(&self) -> u8 {
        MessageType::LinkAdd as u8
    }

    fn remove_message_type(&self) -> u8 {
        MessageType::LinkRemove as u8
    }

    fn compact_state_message_type(&self) -> u8 {
        MessageType::LinkCompactState as u8
    }

    fn is_add_type(&self, message: &Message) -> bool {
        message.signature_scheme == SignatureScheme::Ed25519 as i32
            && message.data.is_some()
            && message.data.as_ref().is_some_and(|data| {
                data.r#type == MessageType::LinkAdd as i32 && data.body.is_some()
            })
    }

    fn is_remove_type(&self, message: &Message) -> bool {
        message.signature_scheme == SignatureScheme::Ed25519 as i32
            && message.data.is_some()
            && message.data.as_ref().is_some_and(|data| {
                data.r#type == MessageType::LinkRemove as i32 && data.body.is_some()
            })
    }

    fn is_compact_state_type(&self, message: &Message) -> bool {
        message.signature_scheme == SignatureScheme::Ed25519 as i32
            && message.data.is_some()
            && message.data.as_ref().is_some_and(|data| {
                data.r#type == MessageType::LinkCompactState as i32 && data.body.is_some()
            })
    }

    fn build_secondary_indices(
        &self,
        txn: &mut RocksDbTransactionBatch,
        ts_hash: &[u8; TS_HASH_LENGTH],
        message: &Message,
    ) -> Result<(), HubError> {
        let (by_target_key, rtype) = self.secondary_index_key(ts_hash, message)?;

        txn.put(by_target_key, rtype);

        Ok(())
    }

    fn delete_secondary_indices(
        &self,
        txn: &mut RocksDbTransactionBatch,
        ts_hash: &[u8; TS_HASH_LENGTH],
        message: &Message,
    ) -> Result<(), HubError> {
        let (by_target_key, _) = self.secondary_index_key(ts_hash, message)?;
        if self.is_add_type(message) {
            let incorrectly_padded_key = Self::make_add_key_padded(message, false)?;
            txn.delete(incorrectly_padded_key);
        } else if self.is_remove_type(message) {
            let incorrectly_padded_key = Self::make_remove_key_padded(message, false)?;
            txn.delete(incorrectly_padded_key);
        };

        txn.delete(by_target_key);

        Ok(())
    }

    fn delete_remove_secondary_indices(
        &self,
        txn: &mut RocksDbTransactionBatch,
        message: &Message,
    ) -> Result<(), HubError> {
        if self.is_add_type(message) {
            let incorrectly_padded_key = Self::make_add_key_padded(message, false)?;
            txn.delete(incorrectly_padded_key);
        } else if self.is_remove_type(message) {
            let incorrectly_padded_key = Self::make_remove_key_padded(message, false)?;
            txn.delete(incorrectly_padded_key);
        };

        Ok(())
    }

    // During the initial rust migration, we were not padding the type field to 8 bytes, so we still
    // have some links that don't have the right padding. Override the default merge conflict resolution
    // to check for the presence of incorrectly padded links as well
    fn get_merge_conflicts(
        &self,
        db: &RocksDB,
        message: &Message,
        ts_hash: &[u8; TS_HASH_LENGTH],
    ) -> Result<Vec<Message>, HubError> {
        // First, call the default implementation to get the default merge conflicts
        let mut conflicts = Self::get_default_merge_conflicts(self, db, message, ts_hash)?;

        let remove_key = Self::make_remove_key_padded(message, false)?;
        let remove_ts_hash = db.get(&remove_key)?;

        if remove_ts_hash.is_some() {
            let remove_compare = self.message_compare(
                self.remove_message_type(),
                &remove_ts_hash.clone().unwrap(),
                message.data.as_ref().unwrap().r#type as u8,
                &ts_hash.to_vec(),
            );

            if remove_compare > 0 {
                return Err(HubError {
                    code: "bad_request.conflict".to_string(),
                    message: "message conflicts with a more recent remove".to_string(),
                });
            }
            if remove_compare == 0 {
                return Err(HubError {
                    code: "bad_request.duplicate".to_string(),
                    message: "message has already been merged".to_string(),
                });
            }

            // If the existing remove has a lower order than the new message, retrieve the full
            // Remove message and delete it as part of the RocksDB transaction
            let maybe_existing_remove = get_message(
                &db,
                message.data.as_ref().unwrap().fid as u32,
                self.postfix(),
                &vec_to_u8_24(&remove_ts_hash)?,
            )?;

            if maybe_existing_remove.is_some() {
                conflicts.push(maybe_existing_remove.unwrap());
            } else {
                warn!(
                    "Message's ts_hash exists but message not found in store {:#?}",
                    remove_ts_hash
                );
            }
        }

        // Check if there is an add timestamp hash for this
        let add_key = Self::make_add_key_padded(message, false)?;
        let add_ts_hash = db.get(&add_key)?;

        if add_ts_hash.is_some() {
            let add_compare = self.message_compare(
                self.add_message_type(),
                &add_ts_hash.clone().unwrap(),
                message.data.as_ref().unwrap().r#type as u8,
                &ts_hash.to_vec(),
            );

            if add_compare > 0 {
                return Err(HubError {
                    code: "bad_request.conflict".to_string(),
                    message: "message conflicts with a more recent add".to_string(),
                });
            }
            if add_compare == 0 {
                return Err(HubError {
                    code: "bad_request.duplicate".to_string(),
                    message: "message has already been merged".to_string(),
                });
            }

            // If the existing add has a lower order than the new message, retrieve the full
            // Add message and delete it as part of the RocksDB transaction
            let maybe_existing_add = get_message(
                &db,
                message.data.as_ref().unwrap().fid as u32,
                self.postfix(),
                &vec_to_u8_24(&add_ts_hash)?,
            )?;

            if maybe_existing_add.is_none() {
                warn!(
                    "Message's ts_hash exists but message not found in store {:#?}",
                    add_ts_hash
                );
            } else {
                conflicts.push(maybe_existing_add.unwrap());
            }
        }

        return Ok(conflicts);
    }

    fn find_merge_add_conflicts(&self, _db: &RocksDB, _message: &Message) -> Result<(), HubError> {
        // For links, there will be no additional conflict logic
        Ok(())
    }

    fn find_merge_remove_conflicts(
        &self,
        _db: &RocksDB,
        _message: &Message,
    ) -> Result<(), HubError> {
        // For links, there will be no additional conflict logic
        Ok(())
    }

    fn make_compact_state_add_key(&self, message: &Message) -> Result<Vec<u8>, HubError> {
        message
            .data
            .as_ref()
            .ok_or(HubError::invalid_parameter("invalid message data"))
            .and_then(|data| {
                data.body
                    .as_ref()
                    .ok_or(HubError::invalid_parameter("invalid message data body"))
                    .and_then(|body_option| match body_option {
                        Body::LinkCompactStateBody(link_compact_body) => {
                            Self::link_compact_state_add_key(
                                data.fid as u32,
                                &link_compact_body.r#type,
                            )
                        }
                        Body::LinkBody(link_body) => {
                            Self::link_compact_state_add_key(data.fid as u32, &link_body.r#type)
                        }
                        _ => Err(HubError::invalid_parameter(
                            "link_compact_body not specified",
                        )),
                    })
            })
    }

    fn make_compact_state_prefix(&self, fid: u32) -> Result<Vec<u8>, HubError> {
        let mut prefix =
            Vec::with_capacity(Self::ROOT_PREFIXED_FID_BYTE_SIZE + Self::POSTFIX_BYTE_SIZE);

        prefix.extend_from_slice(&make_user_key(fid));
        prefix.push(UserPostfix::LinkCompactStateMessage.as_u8());

        Ok(prefix)
    }

    fn make_add_key(&self, message: &Message) -> Result<Vec<u8>, HubError> {
        // Type bytes must be padded to 8 bytes, but we had a bug which allowed unpadded types,
        // so this function allows access to both types of keys
        return Self::make_add_key_padded(message, true);
    }

    fn make_remove_key(&self, message: &Message) -> Result<Vec<u8>, HubError> {
        return Self::make_remove_key_padded(message, true);
    }

    fn get_prune_size_limit(&self) -> u32 {
        self.prune_size_limit
    }
}
