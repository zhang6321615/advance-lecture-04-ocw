//! A demonstration of an offchain worker that sends onchain callbacks

#![cfg_attr(not(feature = "std"), no_std)]

use core::fmt;
#[allow(unused_imports)]
use num_traits::float::FloatCore;
use frame_support::{
	debug, decl_error, decl_event, decl_module, decl_storage, dispatch::DispatchResult,
	traits::Get,
};
use parity_scale_codec::{Decode, Encode};

use frame_system::{
	self as system, ensure_none,
	offchain::{
		AppCrypto, CreateSignedTransaction, SendUnsignedTransaction,
		SignedPayload, SigningTypes, Signer,
	},
};
use sp_core::crypto::KeyTypeId;
use sp_runtime::{
	RuntimeDebug,
	offchain as rt_offchain,
	offchain::{
		storage_lock::{StorageLock, BlockAndTime},
	},
	transaction_validity::{
		InvalidTransaction, TransactionSource, TransactionValidity,
		ValidTransaction,
	},
};
use sp_std::{
	prelude::*, str,
	collections::vec_deque::VecDeque,
};

use serde::{Deserialize, Deserializer};

/// Defines application identifier for crypto keys of this module.
///
/// Every module that deals with signatures needs to declare its unique identifier for
/// its crypto keys.
/// When an offchain worker is signing transactions it's going to request keys from type
/// `KeyTypeId` via the keystore to sign the transaction.
/// The keys can be inserted manually via RPC (see `author_insertKey`).
pub const KEY_TYPE: KeyTypeId = KeyTypeId(*b"demo");
pub const NUM_VEC_LEN: usize = 10;
/// The type to sign and send transactions.
pub const UNSIGNED_TXS_PRIORITY: u64 = 100;

// We are fetching information from the github public API about organization`substrate-developer-hub`.
pub const HTTP_REMOTE_REQUEST: &str = "https://api.coincap.io/v2/assets/polkadot";
pub const HTTP_HEADER_USER_AGENT: &str = "MrPai";

pub const FETCH_TIMEOUT_PERIOD: u64 = 3000;
// in milli-seconds
pub const LOCK_TIMEOUT_EXPIRATION: u64 = FETCH_TIMEOUT_PERIOD + 1000;
// in milli-seconds
// 如果超过2/3的validator已经发送了报价，那么再经过3个区块高度，开始计算当前轮次的平均价格
pub const LOCK_BLOCK_EXPIRATION: u32 = 3; // in block number

/// Based on the above `KeyTypeId` we need to generate a pallet-specific crypto type wrapper.
/// We can utilize the supported crypto kinds (`sr25519`, `ed25519` and `ecdsa`) and augment
/// them with the pallet-specific identifier.
pub mod crypto {
	use crate::KEY_TYPE;
	use sp_core::sr25519::Signature as Sr25519Signature;
	use sp_runtime::app_crypto::{app_crypto, sr25519};
	use sp_runtime::{
		traits::Verify,
		MultiSignature, MultiSigner,
	};

	app_crypto!(sr25519, KEY_TYPE);

	pub struct TestAuthId;

	// implemented for ocw-runtime
	impl frame_system::offchain::AppCrypto<MultiSigner, MultiSignature> for TestAuthId {
		type RuntimeAppPublic = Public;
		type GenericSignature = sp_core::sr25519::Signature;
		type GenericPublic = sp_core::sr25519::Public;
	}

	// implemented for mock runtime in test
	impl frame_system::offchain::AppCrypto<<Sr25519Signature as Verify>::Signer, Sr25519Signature>
	for TestAuthId
	{
		type RuntimeAppPublic = Public;
		type GenericSignature = sp_core::sr25519::Signature;
		type GenericPublic = sp_core::sr25519::Public;
	}
}

#[derive(Encode, Decode, Clone, PartialEq, Eq, RuntimeDebug)]
pub struct Payload<Public> {
	price: Price,
	public: Public,
}

impl<T: SigningTypes> SignedPayload<T> for Payload<T::Public> {
	fn public(&self) -> T::Public {
		self.public.clone()
	}
}

pub type Price = u64;

#[derive(Deserialize, Encode, Decode, Default)]
struct AssertToken {
	data: DataDetail,
	timestamp: u64,
}

#[allow(non_snake_case)]
#[derive(Deserialize, Encode, Decode, Default)]
struct DataDetail {
	#[serde(deserialize_with = "de_string_to_bytes")]
	id: Vec<u8>,
	#[serde(deserialize_with = "de_string_to_bytes")]
	symbol: Vec<u8>,
	#[serde(deserialize_with = "de_string_to_bytes")]
	priceUsd: Vec<u8>,
}

pub fn de_string_to_bytes<'de, D>(de: D) -> Result<Vec<u8>, D::Error>
	where
		D: Deserializer<'de>,
{
	let s: &str = Deserialize::deserialize(de)?;
	Ok(s.as_bytes().to_vec())
}

impl fmt::Debug for AssertToken {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(
			f,
			"{{ data: {:?}, timestamp: {} }}",
			&self.data,
			&self.timestamp
		)
	}
}

impl fmt::Debug for DataDetail {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(
			f,
			"{{ id: {}, symbol: {}, priceUsd: {} }}",
			str::from_utf8(&self.id).map_err(|_| fmt::Error)?,
			str::from_utf8(&self.symbol).map_err(|_| fmt::Error)?,
			str::from_utf8(&self.priceUsd).map_err(|_| fmt::Error)?
		)
	}
}

/// This is the pallet's configuration trait
pub trait Trait: system::Trait + CreateSignedTransaction<Call<Self>> {
	/// The identifier type for an offchain worker.
	type AuthorityId: AppCrypto<Self::Public, Self::Signature>;
	/// The overarching dispatch call type.
	type Call: From<Call<Self>>;
	/// The overarching event type.
	type Event: From<Event<Self>> + Into<<Self as system::Trait>::Event>;
	/// 用来表示小数位精度，每一个f64将乘以这个常量后转化为u64，剩下的小数位将舍弃
	type PricePrecision: Get<u8>;
}

decl_storage! {
	trait Store for Module<T: Trait> as Example {
		/// 储存价格的双向链表，只保留NUM_VEC_LEN位
		Prices get(fn get_price): VecDeque<Price>;
	}
}

decl_event!(
	/// Events generated by the module.
	pub enum Event<T>
	where
		AccountId = <T as system::Trait>::AccountId,
	{
		/// 节点Id，提交价格
		ReceivedPrice(Option<AccountId>, Price),
	}
);

decl_error! {
	pub enum Error for Module<T: Trait> {
		// Error returned when not sure which ocw function to executed
		UnknownOffchainMux,

		// Error returned when making signed transactions in off-chain worker
		NoLocalAcctForSigning,
		OffchainSignedTxError,

		// Error returned when making unsigned transactions in off-chain worker
		OffchainUnsignedTxError,

		// Error returned when making unsigned transactions with signed payloads in off-chain worker
		OffchainUnsignedTxSignedPayloadError,

		// 网络请求错误
		HttpFetchingError,

		//
		AcquireStorageLockError,
		ConvertToStringError,
		ParsingToF64Error,
	}
}

decl_module! {
	pub struct Module<T: Trait> for enum Call where origin: T::Origin {
		fn deposit_event() = default;

		const PricePrecision: u8 = T::PricePrecision::get();

		#[weight = 10000]
		pub fn submit_price_unsigned_with_signed_payload(origin, payload: Payload<T::Public>,
			_signature: T::Signature) -> DispatchResult
		{
			// Price Oracle
			// 理论上，预言机功能由多个认证节点参与，因此必须包含签名数据，知道是谁提交的；
			// 另外，预言机服务并不一定要交易手续费，因此使用“unsigned_with_signed_payload”.
			let _ = ensure_none(origin)?;
			// we don't need to verify the signature here because it has been verified in
			//   `validate_unsigned` function when sending out the unsigned tx.
			let Payload { price, public } = payload;
			debug::info!("submit_price_unsigned_with_signed_payload: ({}, {:?})", price, public);
			Self::append_or_replace_price(price);

			Self::deposit_event(Event::<T>::ReceivedPrice(None, price));
			Ok(())
		}

		fn offchain_worker(block_number: T::BlockNumber) {
			debug::info!("Entering off-chain worker");
			match Self::fetch_dot_usd_price() {
				Ok(json) => {
					let _ = Self::offchain_price_unsigned_with_signed_payload(json);
				},
				Err(e) => {
					debug::error!("offchain_worker error: {:?}", e);
				}
			}
		}
	}
}

impl<T: Trait> Module<T> {

	// 将价格缓存
	fn append_or_replace_price(price: Price) {
		Prices::mutate(|prices| {
			if prices.len() == NUM_VEC_LEN {
				let _ = prices.pop_front();
			}
			prices.push_back(price);
			debug::info!("price vector: {:?}", prices);
		});
	}

	// 获取dot的价格
	fn fetch_dot_usd_price() -> Result<AssertToken, Error<T>> {
		// 加锁
		let lock_key = b"offchain-demo::lock";
		let mut lock = StorageLock::<BlockAndTime<Self>>::with_block_and_time_deadline(
			lock_key, LOCK_BLOCK_EXPIRATION,
			rt_offchain::Duration::from_millis(LOCK_TIMEOUT_EXPIRATION),
		);
		if let Ok(_guard) = lock.try_lock() {
			return match Self::fetch_n_parse() {
				Ok(gh_info) => {
					Ok(gh_info)
				}
				Err(err) => {
					Err(err)
				}
			}
		}
		Err(Error::<T>::AcquireStorageLockError.into())
	}

	// 获取数据并解析
	fn fetch_n_parse() -> Result<AssertToken, Error<T>> {
		// 请求数据源
		let resp_bytes = Self::fetch_from_remote().map_err(|e| {
			debug::error!("fetch_from_remote error: {:?}", e);
			Error::<T>::HttpFetchingError
		})?;

		// 字节数组转字符串
		let resp_str = str::from_utf8(&resp_bytes).map_err(|_| <Error<T>>::HttpFetchingError)?;

		// 打印返回的字符串
		debug::info!("{}", resp_str);

		// 字符串反序列化为结构体
		let data_info: AssertToken = serde_json::from_str(&resp_str).map_err(|_| <Error<T>>::HttpFetchingError)?;

		debug::info!("{:?}", str::from_utf8(&data_info.data.priceUsd));

		Ok(data_info)
	}

	fn fetch_from_remote() -> Result<Vec<u8>, Error<T>> {
		debug::info!("sending request: {}", HTTP_REMOTE_REQUEST);

		// 构建请求
		let request = rt_offchain::http::Request::get(HTTP_REMOTE_REQUEST);

		// 构建超时
		let timeout = sp_io::offchain::timestamp()
			.add(rt_offchain::Duration::from_millis(FETCH_TIMEOUT_PERIOD));

		let pending = request
			.deadline(timeout) // 设置超时时间
			.send() // 发送请求
			.map_err(|_| Error::<T>::HttpFetchingError)?;

		// 等待请求返回
		let response = pending
			.try_wait(timeout)
			.map_err(|_| Error::<T>::HttpFetchingError)?
			.map_err(|_| Error::<T>::HttpFetchingError)?;

		// 判断状态码
		if response.code != 200 {
			debug::error!("Unexpected http request status code: {}", response.code);
			return Err(Error::<T>::HttpFetchingError);
		}

		Ok(response.body().collect::<Vec<u8>>())
	}

	fn offchain_price_unsigned_with_signed_payload(json: AssertToken) -> Result<(), Error<T>> {
		let signer = Signer::<T, T::AuthorityId>::any_account();

		// 将价格转为f64类型
		let price_u8 = json.data.priceUsd;
		let price_f64: f64 = core::str::from_utf8(&price_u8)
			.map_err(|_| {
				Error::<T>::ConvertToStringError
			})?
			.parse::<f64>()
			.map_err(|_| {
				Error::<T>::ParsingToF64Error
			})?;

		// 转换价格u64
		let price = (price_f64 * 10f64.powi(T::PricePrecision::get() as i32)).round() as Price;

		// 调用无签名交易,将数据存入存储
		if let Some((_, res)) = signer.send_unsigned_transaction(
			|acct| Payload { price, public: acct.public.clone() },
			Call::submit_price_unsigned_with_signed_payload) {
			return res.map_err(|_| {
				debug::error!("Failed in offchain_unsigned_tx_signed_payload");
				Error::<T>::OffchainUnsignedTxSignedPayloadError
			});
		}

		// The case of `None`: 无账户可用
		debug::error!("No local account available");

		Err(Error::<T>::NoLocalAcctForSigning)
	}
}

impl<T: Trait> frame_support::unsigned::ValidateUnsigned for Module<T> {
	type Call = Call<T>;

	fn validate_unsigned(_source: TransactionSource, call: &Self::Call) -> TransactionValidity {
		let valid_tx = |provide| ValidTransaction::with_tag_prefix("ocw-demo")
			.priority(UNSIGNED_TXS_PRIORITY)
			.and_provides([&provide])
			.longevity(3)
			.propagate(true)
			.build();

		match call {
			Call::submit_price_unsigned_with_signed_payload(ref payload, ref signature) => {
				if !SignedPayload::<T>::verify::<T::AuthorityId>(payload, signature.clone()) {
					return InvalidTransaction::BadProof.into();
				}
				valid_tx(b"submit_price_unsigned_with_signed_payload".to_vec())
			}
			_ => InvalidTransaction::Call.into(),
		}
	}
}

impl<T: Trait> rt_offchain::storage_lock::BlockNumberProvider for Module<T> {
	type BlockNumber = T::BlockNumber;
	fn current_block_number() -> Self::BlockNumber {
		<frame_system::Module<T>>::block_number()
	}
}
