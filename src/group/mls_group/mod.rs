use log::{debug, info};

mod apply_commit;
mod create_commit;
mod new_from_welcome;

use crate::ciphersuite::*;
use crate::codec::*;
use crate::config::{Config, ConfigError};
use crate::creds::CredentialBundle;
use crate::framing::*;
use crate::group::*;
use crate::key_packages::*;
use crate::messages::{proposals::*, *};
use crate::schedule::*;
use crate::tree::{index::*, node::*, secret_tree::*, *};

use std::cell::{Ref, RefCell};
use std::convert::TryFrom;

pub type CreateCommitResult =
    Result<(MLSPlaintext, Option<Welcome>, Option<KeyPackageBundle>), CreateCommitError>;

pub struct MlsGroup {
    ciphersuite: &'static Ciphersuite,
    group_context: GroupContext,
    epoch_secrets: EpochSecrets,
    secret_tree: RefCell<SecretTree>,
    tree: RefCell<RatchetTree>,
    interim_transcript_hash: Vec<u8>,
    // Group config.
    // Set to true if the ratchet tree extension is added to the `GroupInfo`.
    // Defaults to `false`.
    add_ratchet_tree_extension: bool,
}

/// Public `MlsGroup` functions.
impl MlsGroup {
    pub fn new(
        id: &[u8],
        ciphersuite_name: CiphersuiteName,
        key_package_bundle: KeyPackageBundle,
        config: GroupConfig,
    ) -> Result<Self, ConfigError> {
        info!("Created group {:x?}", id);
        debug!(" >>> with {:?}, {:?}", ciphersuite_name, config);
        let group_id = GroupId { value: id.to_vec() };
        let epoch_secrets = EpochSecrets::default();
        let ciphersuite = Config::ciphersuite(ciphersuite_name)?;
        let secret_tree = SecretTree::new(&epoch_secrets.encryption_secret, LeafIndex::from(1u32));
        let tree = RatchetTree::new(ciphersuite, key_package_bundle);
        let group_context = GroupContext {
            group_id,
            epoch: GroupEpoch(0),
            tree_hash: tree.compute_tree_hash(),
            confirmed_transcript_hash: vec![],
        };
        let interim_transcript_hash = vec![];
        Ok(MlsGroup {
            ciphersuite,
            group_context,
            epoch_secrets,
            secret_tree: RefCell::new(secret_tree),
            tree: RefCell::new(tree),
            interim_transcript_hash,
            add_ratchet_tree_extension: config.add_ratchet_tree_extension,
        })
    }

    // Join a group from a welcome message
    pub fn new_from_welcome(
        welcome: Welcome,
        nodes_option: Option<Vec<Option<Node>>>,
        kpb: KeyPackageBundle,
    ) -> Result<Self, WelcomeError> {
        Self::new_from_welcome_internal(welcome, nodes_option, kpb)
    }

    // === Create handshake messages ===
    // TODO: share functionality between these.

    // 11.1.1. Add
    // struct {
    //     KeyPackage key_package;
    // } Add;
    pub fn create_add_proposal(
        &self,
        aad: &[u8],
        credential_bundle: &CredentialBundle,
        joiner_key_package: KeyPackage,
    ) -> MLSPlaintext {
        let add_proposal = AddProposal {
            key_package: joiner_key_package,
        };
        let proposal = Proposal::Add(add_proposal);
        let content = MLSPlaintextContentType::Proposal(proposal);
        MLSPlaintext::new(
            self.sender_index(),
            aad,
            content,
            credential_bundle,
            &self.context(),
        )
    }

    // 11.1.2. Update
    // struct {
    //     KeyPackage key_package;
    // } Update;
    pub fn create_update_proposal(
        &self,
        aad: &[u8],
        credential_bundle: &CredentialBundle,
        key_package: KeyPackage,
    ) -> MLSPlaintext {
        let update_proposal = UpdateProposal { key_package };
        let proposal = Proposal::Update(update_proposal);
        let content = MLSPlaintextContentType::Proposal(proposal);
        MLSPlaintext::new(
            self.sender_index(),
            aad,
            content,
            credential_bundle,
            &self.context(),
        )
    }

    // 11.1.3. Remove
    // struct {
    //     uint32 removed;
    // } Remove;
    pub fn create_remove_proposal(
        &self,
        aad: &[u8],
        credential_bundle: &CredentialBundle,
        removed_index: LeafIndex,
    ) -> MLSPlaintext {
        let remove_proposal = RemoveProposal {
            removed: removed_index.into(),
        };
        let proposal = Proposal::Remove(remove_proposal);
        let content = MLSPlaintextContentType::Proposal(proposal);
        MLSPlaintext::new(
            self.sender_index(),
            aad,
            content,
            credential_bundle,
            &self.context(),
        )
    }

    // === ===

    // 11.2. Commit
    // opaque ProposalID<0..255>;
    //
    // struct {
    //     ProposalID proposals<0..2^32-1>;
    //     optional<UpdatePath> path;
    // } Commit;
    pub fn create_commit(
        &self,
        aad: &[u8],
        credential_bundle: &CredentialBundle,
        proposals: Vec<MLSPlaintext>,
        force_self_update: bool,
    ) -> CreateCommitResult {
        self.create_commit_internal(aad, credential_bundle, proposals, force_self_update)
    }

    // Apply a Commit message
    pub fn apply_commit(
        &mut self,
        mls_plaintext: MLSPlaintext,
        proposals: Vec<MLSPlaintext>,
        own_key_packages: &[KeyPackageBundle],
    ) -> Result<(), ApplyCommitError> {
        self.apply_commit_internal(mls_plaintext, proposals, own_key_packages)
    }

    // Create application message
    pub fn create_application_message(
        &mut self,
        aad: &[u8],
        msg: &[u8],
        credential_bundle: &CredentialBundle,
    ) -> MLSCiphertext {
        let content = MLSPlaintextContentType::Application(msg.to_vec());
        let mls_plaintext = MLSPlaintext::new(
            self.sender_index(),
            aad,
            content,
            credential_bundle,
            &self.context(),
        );
        self.encrypt(mls_plaintext)
    }

    // Encrypt/Decrypt MLS message
    pub fn encrypt(&mut self, mls_plaintext: MLSPlaintext) -> MLSCiphertext {
        let mut secret_tree = self.secret_tree.borrow_mut();
        let secret_type = SecretType::try_from(&mls_plaintext).unwrap();
        let (generation, (ratchet_key, ratchet_nonce)) = secret_tree.get_secret_for_encryption(
            self.ciphersuite(),
            mls_plaintext.sender.sender,
            secret_type,
        );
        MLSCiphertext::new_from_plaintext(
            &mls_plaintext,
            &self,
            generation,
            ratchet_key,
            ratchet_nonce,
        )
    }

    pub fn decrypt(&mut self, mls_ciphertext: MLSCiphertext) -> Result<MLSPlaintext, GroupError> {
        let tree = self.tree.borrow();
        let mut roster = Vec::new();
        for i in 0..tree.leaf_count().as_usize() {
            let node = &tree.nodes[LeafIndex::from(i)];
            let credential = if let Some(kp) = &node.key_package {
                kp.credential()
            } else {
                panic!("Missing key package");
            };
            roster.push(credential);
        }

        Ok(mls_ciphertext.to_plaintext(
            self.ciphersuite(),
            &roster,
            &self.epoch_secrets,
            &mut self.secret_tree.borrow_mut(),
            &self.group_context,
        )?)
    }

    // Exporter
    pub fn export_secret(&self, label: &str, key_length: usize) -> Secret {
        mls_exporter(
            self.ciphersuite(),
            &self.epoch_secrets,
            label,
            &self.context(),
            key_length,
        )
    }
}

impl MlsGroup {
    pub fn tree(&self) -> Ref<RatchetTree> {
        self.tree.borrow()
    }
    fn sender_index(&self) -> LeafIndex {
        self.tree.borrow().get_own_node_index().into()
    }

    /// Get the ciphersuite implementation used in this group.
    pub fn ciphersuite(&self) -> &Ciphersuite {
        self.ciphersuite
    }

    pub fn context(&self) -> &GroupContext {
        &self.group_context
    }

    pub fn group_id(&self) -> GroupId {
        self.group_context.group_id.clone()
    }

    pub(crate) fn epoch_secrets(&self) -> &EpochSecrets {
        &self.epoch_secrets
    }
}

// Helper functions

fn update_confirmed_transcript_hash(
    ciphersuite: &Ciphersuite,
    mls_plaintext_commit_content: &MLSPlaintextCommitContent,
    interim_transcript_hash: &[u8],
) -> Vec<u8> {
    let commit_content_bytes = mls_plaintext_commit_content.serialize();
    ciphersuite.hash(&[interim_transcript_hash, &commit_content_bytes].concat())
}

fn update_interim_transcript_hash(
    ciphersuite: &Ciphersuite,
    mls_plaintext_commit_auth_data: &MLSPlaintextCommitAuthData,
    confirmed_transcript_hash: &[u8],
) -> Vec<u8> {
    let commit_auth_data_bytes = mls_plaintext_commit_auth_data.serialize();
    ciphersuite.hash(&[confirmed_transcript_hash, &commit_auth_data_bytes].concat())
}

fn compute_welcome_key_nonce(
    ciphersuite: &Ciphersuite,
    joiner_secret: &Secret,
) -> (AeadKey, AeadNonce) {
    let welcome_secret = ciphersuite
        .hkdf_expand(joiner_secret, b"mls 1.0 welcome", ciphersuite.hash_length())
        .unwrap();
    let welcome_nonce = AeadNonce::from_secret(
        ciphersuite
            .hkdf_expand(&welcome_secret, b"nonce", ciphersuite.aead_nonce_length())
            .unwrap(),
    );
    let welcome_key = AeadKey::from_secret(
        ciphersuite
            .hkdf_expand(&welcome_secret, b"key", ciphersuite.aead_key_length())
            .unwrap(),
        ciphersuite.aead(),
    );
    (welcome_key, welcome_nonce)
}
