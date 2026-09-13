// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The error codes `wrkz-service` answers with.
//!
//! Every handler failure comes back the same shape
//! (`JsonRpcServer::makeErrorResponse`, `jsonrpcserver/JsonRpcServer.cpp:118`):
//! `{"error": {"code": -32700, "message": "...", "data": {"application_code": N}}}`.
//!
//! **`code` is `-32700` for every application error**, whatever went wrong:
//! `makeErrorResponse` writes `errParseError` unconditionally. It is the
//! `application_code` and the message that say what happened. Do not fix it —
//! integrations written against `wrkz-service` read those two fields.
//!
//! `application_code` is `std::error_code::value()`, and the C++ raises codes
//! from two different categories through the same field:
//!
//! - `CryptoNote::error::WalletErrorCodes` (`wallet/WalletErrors.h`), 1..=35,
//!   which is [`Wallet`];
//! - `CryptoNote::error::WalletServiceErrorCode`
//!   (`walletservice/WalletServiceErrorCategory.h`), 1..=6, which is
//!   [`Service`].
//!
//! The two overlap numerically — `1` is `NOT_INITIALIZED` in one and
//! `WRONG_KEY_FORMAT` in the other — and the wire carries no category, so only
//! the message tells them apart. That is the deployed behaviour, reproduced.

/// `CryptoNote::JsonRpc::errParseError` (`rpc/JsonRpc.h:23`), the `code` of
/// every application error and of a malformed request body.
/// The one message with an apostrophe in it, kept verbatim from
/// `WalletErrors.h:139`.
const CONFLICTING_PAYMENT_IDS_MESSAGE: &str =
    "Multiple conflicting payment ID's were specified via the use of integrated addresses";

pub const ERR_PARSE_ERROR: i64 = -32700;
/// `errMethodNotFound` (`:27`).
pub const ERR_METHOD_NOT_FOUND: i64 = -32601;
/// `errInvalidPassword` (`:33`).
pub const ERR_INVALID_PASSWORD: i64 = -32604;
/// The literal `-3600` `processJsonRpcRequest` sends when `method` is missing
/// or is not a string (`PaymentServiceJsonRpcServer.cpp:212`). Note that it is
/// not `errInvalidRequest` (-32600): the C++ passes -3600 by hand.
pub const ERR_INVALID_REQUEST: i64 = -3600;

/// `CryptoNote::error::WalletErrorCodes` (`wallet/WalletErrors.h:18`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i64)]
pub enum Wallet {
    NotInitialized = 1,
    AlreadyInitialized = 2,
    WrongState = 3,
    WrongPassword = 4,
    InternalWalletError = 5,
    MixinCountTooBig = 6,
    BadAddress = 7,
    TransactionSizeTooBig = 8,
    WrongAmount = 9,
    SumOverflow = 10,
    ZeroDestination = 11,
    TxCancelImpossible = 12,
    TxCancelled = 13,
    OperationCancelled = 14,
    TxTransferImpossible = 15,
    WrongVersion = 16,
    FeeTooSmall = 17,
    KeyGenerationError = 18,
    IndexOutOfRange = 19,
    AddressAlreadyExists = 20,
    TrackingMode = 21,
    WrongParameters = 22,
    ObjectNotFound = 23,
    WalletNotFound = 24,
    ChangeAddressRequired = 25,
    ChangeAddressNotFound = 26,
    DestinationAddressRequired = 27,
    DestinationAddressNotFound = 28,
    BadPaymentId = 29,
    BadTransactionExtra = 30,
    MixinBelowThreshold = 31,
    MixinAboveThreshold = 32,
    ConflictingPaymentIds = 33,
    ExtraTooLarge = 34,
    ExcessiveOutputs = 35,
}

impl Wallet {
    /// `WalletErrorCategory::message` (`WalletErrors.h:72`), word for word.
    pub fn message(self) -> &'static str {
        match self {
            Wallet::NotInitialized => "Object was not initialized",
            Wallet::WrongPassword => "The password is wrong",
            Wallet::AlreadyInitialized => "The object is already initialized",
            Wallet::InternalWalletError => "Internal error occurred",
            Wallet::MixinCountTooBig => "MixIn count is too big",
            Wallet::BadAddress => "Bad address",
            Wallet::TransactionSizeTooBig => "Transaction size is too big",
            Wallet::WrongAmount => "Wrong amount",
            Wallet::SumOverflow => "Sum overflow",
            Wallet::ZeroDestination => "The destination is empty",
            Wallet::TxCancelImpossible => "Impossible to cancel transaction",
            Wallet::WrongState => "The wallet is in wrong state (maybe loading or saving), try again later",
            Wallet::OperationCancelled => "The operation you've requested has been cancelled",
            Wallet::TxTransferImpossible => "Transaction transfer impossible",
            Wallet::WrongVersion => "Wrong version",
            Wallet::FeeTooSmall => "Transaction fee is too small",
            Wallet::KeyGenerationError => "Cannot generate new key",
            Wallet::IndexOutOfRange => "Index is out of range",
            Wallet::AddressAlreadyExists => "Address already exists",
            Wallet::TrackingMode => "The wallet is in tracking mode",
            Wallet::WrongParameters => "Wrong parameters passed",
            Wallet::ObjectNotFound => "Object not found",
            Wallet::WalletNotFound => "Requested wallet not found",
            Wallet::ChangeAddressRequired => "Change address required",
            Wallet::ChangeAddressNotFound => "Change address not found",
            Wallet::DestinationAddressRequired => "Destination address required",
            Wallet::DestinationAddressNotFound => "Destination address not found",
            Wallet::BadPaymentId => "Wrong payment id format",
            Wallet::BadTransactionExtra => "Wrong transaction extra format",
            Wallet::MixinBelowThreshold => "Mixin below minimum allowed threshold",
            Wallet::MixinAboveThreshold => "Mixin above maximum allowed threshold",
            Wallet::ConflictingPaymentIds => CONFLICTING_PAYMENT_IDS_MESSAGE,
            Wallet::ExtraTooLarge => "Transaction extra too large",
            Wallet::ExcessiveOutputs => "Transaction has an excessive number of outputs for the input count",
            // `TxCancelled` has no arm in the C++ switch and falls through.
            Wallet::TxCancelled => "Unknown error",
        }
    }
}

/// `CryptoNote::error::WalletServiceErrorCode`
/// (`walletservice/WalletServiceErrorCategory.h:19`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i64)]
pub enum Service {
    WrongKeyFormat = 1,
    WrongPaymentIdFormat = 2,
    WrongHashFormat = 3,
    ObjectNotFound = 4,
    DuplicateKey = 5,
    KeysNotDeterministic = 6,
}

impl Service {
    /// `WalletServiceErrorCategory::message` (`:45`), word for word.
    pub fn message(self) -> &'static str {
        match self {
            Service::WrongKeyFormat => "Wrong key format",
            Service::WrongPaymentIdFormat => "Wrong payment id format",
            // Yes, "block id": the C++ uses one code for every hash it parses.
            Service::WrongHashFormat => "Wrong block id format",
            Service::ObjectNotFound => "Requested object not found",
            Service::DuplicateKey => "Duplicate key",
            Service::KeysNotDeterministic => "Keys not deterministic",
        }
    }
}

/// One handler failure: the `application_code` and the message that go with it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppError {
    pub application_code: i64,
    pub message: String,
}

impl AppError {
    pub fn wallet(e: Wallet) -> AppError {
        AppError { application_code: e as i64, message: e.message().to_string() }
    }

    pub fn service(e: Service) -> AppError {
        AppError { application_code: e as i64, message: e.message().to_string() }
    }

    /// A `RequestSerializationError`, which the C++ lets escape the handler and
    /// `processJsonRpcRequest` catches as a generic error carrying `what()`
    /// ("Request error") and, unlike the others, no `data` at all
    /// (`PaymentServiceJsonRpcServer.cpp:238`, `makeGenericErrorReponse`).
    pub fn request() -> AppError {
        AppError { application_code: NO_APPLICATION_CODE, message: "Request error".to_string() }
    }

    /// Whether this is the [`AppError::request`] case, which is written to the
    /// wire without a `data` member.
    pub fn is_request_error(&self) -> bool {
        self.application_code == NO_APPLICATION_CODE
    }
}

/// The `application_code` [`AppError::request`] carries, which is never
/// written: a `RequestSerializationError` becomes a generic error with no
/// `data` member. `-1` cannot collide with a real code, which is 1 or more.
pub const NO_APPLICATION_CODE: i64 = -1;

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for AppError {}

/// What every handler returns.
pub type Result<T> = std::result::Result<T, AppError>;
