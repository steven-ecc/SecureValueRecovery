/*
 * Copyright (C) 2019 Open Whisper Systems
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU Affero General Public License as published by
 * the Free Software Foundation, either version 3 of the License, or
 * (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 * GNU Affero General Public License for more details.
 *
 * You should have received a copy of the GNU Affero General Public License
 * along with this program.  If not, see <http://www.gnu.org/licenses/>.
 */

use std::convert::{TryInto};
use std::sync::{Arc};

use futures::future;
use futures::prelude::*;
use futures::sync::oneshot;
use kbupd_api::entities::*;
use kbupd_api::entities::{BackupId};
use ring;

use super::*;
use super::request_manager::*;
use crate::api::{BackupManager};
use crate::api::auth::signal_user::{SignalUser};
use crate::enclave::error::*;
use crate::protobufs::kbupd::*;

pub struct SignalBackupManager<BackupEnclaveTy> {
    enclave:         BackupEnclaveTy,
    backup_id_key:   Arc<ring::hmac::SigningKey>,
    request_manager: BackupRequestManagerSender,
}

impl<BackupEnclaveTy> SignalBackupManager<BackupEnclaveTy> {
    pub fn new(enclave:         BackupEnclaveTy,
               backup_id_key:   ring::hmac::SigningKey,
               request_manager: BackupRequestManagerSender)
               -> Self {
        Self {
            enclave,
            backup_id_key: Arc::new(backup_id_key),
            request_manager,
        }
    }

    fn user_to_backup_id(&self, user: &SignalUser) -> BackupId {
        let signature = ring::hmac::sign(&self.backup_id_key, user.username.as_bytes());
        signature.as_ref()[..32].try_into().unwrap_or_else(|_| unreachable!())
    }
}

impl<BackupEnclaveTy> BackupManager for SignalBackupManager<BackupEnclaveTy>
where BackupEnclaveTy: BackupEnclave + Send + Clone + 'static,
{
    type User = SignalUser;

    fn get_token(&self, enclave_name: String, user: &SignalUser) -> Box<dyn Future<Item = GetTokenResponse, Error = EnclaveTransactionError> + Send> {
        let backup_id           = self.user_to_backup_id(user);
        let create_backup_reply = self.enclave.create_backup(enclave_name, backup_id);
        let get_token_response  = create_backup_reply.and_then(move |reply: CreateBackupReply| {
            let tries: u32 = reply.tries.unwrap_or(0);
            Ok(GetTokenResponse {
                backupId: backup_id,
                token:    reply.token[..].try_into().unwrap_or_else(|_| unreachable!("token is always 32 bytes")),
                tries:    tries as u16,
            })
        });
        Box::new(get_token_response)
    }

    fn get_attestation(&self, enclave_name: String, _user: &SignalUser, request: RemoteAttestationRequest)
                       -> Box<dyn Future<Item = RemoteAttestationResponse, Error = RemoteAttestationError> + Send>
    {
        self.enclave.get_attestation(enclave_name, request)
    }

    fn put_backup_request(&self, enclave_name: String, user: &SignalUser, request: KeyBackupRequest)
                          -> Box<dyn Future<Item = KeyBackupResponse, Error = KeyBackupError> + Send>
    {
        let backup_id  = self.user_to_backup_id(user);
        let request_id = request.requestId.clone();
        let (tx, rx)   = oneshot::channel();

        let maybe_cached_response = self.request_manager.call(move |request_manager: &mut BackupRequestManager, reply_tx| {
            request_manager.start_request(backup_id, request_id, reply_tx)
        });

        let request_manager = self.request_manager.clone();
        let enclave         = self.enclave.clone();
        let response_result = maybe_cached_response.and_then(move |maybe_cached_response: Option<KeyBackupResponse>| {
            if let Some(cached_response) = maybe_cached_response {
                return future::Either::A(Ok(cached_response).into_future());
            }

            let request_id      = request.requestId.clone();
            let response        = enclave.put_backup_request(enclave_name, backup_id, request);
            let response_result = response.then(move |response_result: Result<KeyBackupResponse, KeyBackupError>| {
                let cache_response_result = response_result.clone();
                let _ignore = request_manager.cast(move |request_manager: &mut BackupRequestManager| {
                    request_manager.finish_request(backup_id, request_id, cache_response_result)
                });
                response_result
            });
            future::Either::B(response_result)
        });
        tokio::spawn(response_result.then(move |response_result: Result<KeyBackupResponse, KeyBackupError>| {
            let _ignore = tx.send(response_result);
            Ok(())
        }));

        let response = rx.then(|rx_result: Result<_, futures::Canceled>| rx_result?);
        Box::new(response)
    }
}

impl<BackupEnclaveTy> Clone for SignalBackupManager<BackupEnclaveTy>
where BackupEnclaveTy: BackupEnclave + Clone,
{
    fn clone(&self) -> Self {
        Self {
            enclave:         self.enclave.clone(),
            backup_id_key:   self.backup_id_key.clone(),
            request_manager: self.request_manager.clone(),
        }
    }
}
