//! # Eigen Trust
//!
//! A library for managing trust in a distributed network with zero-knowledge
//! features.
//!
//! ## Main characteristics:
//! **Self-policing** - the shared ethics of the user population is defined and
//! enforced by the peers themselves and not by some central authority.
//!
//! **Minimal** - computation, infrastructure, storage, and message complexity
//! are reduced to a minimum.
//!
//! **Incorruptible** - Reputation should be obtained by consistent good
//! behavior through several transactions. This is enforced for all users, so no
//! one can cheat the system and obtain a higher reputation. It is also
//! resistant to malicious collectives.
//!
//! ## Implementation
//! The library is implemented according to the original [Eigen Trust paper](http://ilpubs.stanford.edu:8090/562/1/2002-56.pdf).
//! It is developed under the Ethereum Foundation grant.

#![feature(async_closure)]
#![feature(array_zip, array_try_map)]
#![allow(clippy::tabs_in_doc_comments)]
#![deny(
	future_incompatible, nonstandard_style, deprecated, unreachable_code, unreachable_patterns,
	absolute_paths_not_starting_with_crate, unsafe_code, clippy::panic, clippy::unnecessary_cast,
	clippy::cast_lossless, clippy::cast_possible_wrap
)]
#![warn(trivial_casts)]
#![forbid(unsafe_code)]

/// The module for global constants.
pub mod constants;
/// The module for epoch-related calculations, like seconds until the next
/// epoch, current epoch, etc.
pub mod epoch;
/// The module where the error enum is defined
pub mod error;
/// The module for the manager related functionalities, like:
/// - Adding/removing neighbors of peers
/// - Calculating the score of peers
/// - Keeping track of neighbors scores towards us
pub mod manager;
/// Common utility functions used across the crate
pub mod utils;

use constants::{EPOCH_INTERVAL, MAX_NEIGHBORS, NUM_BOOTSTRAP_PEERS, NUM_ITERATIONS};
use eigen_trust_circuit::{
	halo2wrong::{
		curves::{
			bn256::{Bn256, Fr as Bn265Scalar},
			group::ff::PrimeField,
			FieldExt,
		},
		halo2::poly::{commitment::ParamsProver, kzg::commitment::ParamsKZG},
	},
	params::poseidon_bn254_5x5::Params,
	utils::{keygen, random_circuit, to_wide},
};
use epoch::Epoch;
use error::EigenError;
use hyper::{
	body::{aggregate, Buf},
	server::conn::{AddrStream, Http},
	service::{make_service_fn, service_fn},
	Body, Method, Request, Response, StatusCode,
};
use manager::{
	sig::{Signature, SignatureData},
	Manager,
};
use once_cell::sync::Lazy;
use rand::thread_rng;
use serde::{ser::StdError, Deserialize, Serialize};
use serde_json::{from_reader, Error as SerdeError, Result as SerdeResult};
use std::{
	collections::HashMap,
	fmt::{Display, Formatter, Result as FmtResult},
	net::SocketAddr,
	sync::{Arc, Mutex, MutexGuard, PoisonError},
};
use tokio::{
	io::{AsyncRead, AsyncWrite},
	net::TcpListener,
	select,
	time::{self, Duration},
};
use utils::{generate_pk_from_sk, scalar_from_bs58};

const BAD_REQUEST: u16 = 400;
const NOT_FOUND: u16 = 404;
const INTERNAL_SERVER_ERROR: u16 = 500;

#[derive(Debug)]
enum ResponseBody {
	SignatureAddSuccess,
	Score(f64),
	LockError,
	InvalidQuery,
	InvalidRequest,
}

impl ToString for ResponseBody {
	fn to_string(&self) -> String {
		match self {
			ResponseBody::SignatureAddSuccess => "SignatureAddSuccess".to_string(),
			ResponseBody::Score(s) => s.to_string(),
			ResponseBody::LockError => "LockError".to_string(),
			ResponseBody::InvalidQuery => "InvalidQuery".to_string(),
			ResponseBody::InvalidRequest => "InvalidRequest".to_string(),
		}
	}
}

struct Query {
	pk: Bn265Scalar,
	epoch: Epoch,
}

impl Query {
	pub fn parse(query_string: &str) -> Option<Query> {
		let parts: Vec<&str> = query_string.split("&").into_iter().collect();
		if parts.len() != 2 {
			return None;
		}

		let mut map = HashMap::new();
		for part in parts {
			let pair: Vec<&str> = part.split("=").into_iter().collect();
			if pair.len() != 2 {
				return None;
			}
			map.insert(pair[0], pair[1]);
		}

		let pk = map.get("pk");
		let epoch = map.get("epoch");
		if pk.is_none() || epoch.is_none() {
			return None;
		}

		let pk_bytes = bs58::decode(pk.unwrap()).into_vec();
		if pk_bytes.is_err() {
			return None;
		}
		let pk_scalar = Bn265Scalar::from_bytes_wide(&to_wide(&pk_bytes.unwrap()));
		let epoch_res: Result<u64, _> = epoch.unwrap().parse();
		if epoch_res.is_err() {
			return None;
		}
		let epoch = Epoch(epoch_res.unwrap());

		Some(Query { pk: pk_scalar, epoch })
	}
}

static MANAGER_STORE: Lazy<Arc<Mutex<Manager>>> = Lazy::new(|| {
	let mut rng = thread_rng();
	let params = ParamsKZG::new(9);
	let random_circuit =
		random_circuit::<Bn256, _, MAX_NEIGHBORS, NUM_BOOTSTRAP_PEERS, Params>(&mut rng);
	let proving_key = keygen(&params, &random_circuit).unwrap();

	Arc::new(Mutex::new(Manager::new(params, proving_key)))
});

async fn handle_request(
	req: Request<Body>, arc_manager: Arc<Mutex<Manager>>,
) -> Result<Response<String>, EigenError> {
	match (req.method(), req.uri().path()) {
		(&Method::GET, "/score") => {
			let q = req.uri().query();
			if q.is_none() {
				let res = Response::builder()
					.status(BAD_REQUEST)
					.body(ResponseBody::InvalidQuery.to_string())
					.unwrap();
				return Ok(res);
			}
			let query_string = q.unwrap();
			let query = Query::parse(query_string);
			if query.is_none() {
				let res = Response::builder()
					.status(BAD_REQUEST)
					.body(ResponseBody::InvalidQuery.to_string())
					.unwrap();
				return Ok(res);
			}
			let query = query.unwrap();
			let manager = arc_manager.lock();
			if manager.is_err() {
				let res = Response::builder()
					.status(INTERNAL_SERVER_ERROR)
					.body(ResponseBody::LockError.to_string())
					.unwrap();
				return Ok(res);
			}
			let m = manager.unwrap();
			let sig_res = m.get_signature(&query.pk);
			if sig_res.is_err() {
				let res = Response::builder()
					.status(BAD_REQUEST)
					.body(ResponseBody::InvalidQuery.to_string())
					.unwrap();
				return Ok(res);
			}
			let sig = sig_res.unwrap();
			let ops = m.get_op_jis(sig, query.epoch, NUM_ITERATIONS);
			let ops_sum: f64 = ops.iter().sum();
			let res = Response::new(ResponseBody::Score(ops_sum).to_string());
			return Ok(res);
		},
		(&Method::POST, "/signature") => {
			// Aggregate the body...
			let whole_body = aggregate(req).await;
			if whole_body.is_err() {
				let res = Response::builder()
					.status(BAD_REQUEST)
					.body(ResponseBody::InvalidRequest.to_string())
					.unwrap();
				return Ok(res);
			}
			let whole_body = whole_body.unwrap();
			// Decode as JSON...
			let data: SerdeResult<SignatureData> = from_reader(whole_body.reader());
			if data.is_err() {
				let res = Response::builder()
					.status(BAD_REQUEST)
					.body(ResponseBody::InvalidRequest.to_string())
					.unwrap();
				return Ok(res);
			}
			let manager = arc_manager.lock();
			if manager.is_err() {
				let res = Response::builder()
					.status(INTERNAL_SERVER_ERROR)
					.body(ResponseBody::LockError.to_string())
					.unwrap();
				return Ok(res);
			}
			let mut m = manager.unwrap();
			let data = data.unwrap();
			let sig: Signature = data.clone().into();
			m.add_signature(sig);
			let res = ResponseBody::SignatureAddSuccess;
			return Ok(Response::new(res.to_string()));
		},
		_ => {
			return Ok(Response::builder()
				.status(NOT_FOUND)
				.body(ResponseBody::InvalidRequest.to_string())
				.unwrap())
		},
	}
}

async fn handle_connection<I: AsyncRead + AsyncWrite + Unpin + 'static>(
	stream: I, _addr: SocketAddr,
) {
	let mut https = Http::new();
	https.http1_keep_alive(false);

	let service_function = service_fn(async move |req| {
		let mng_store = Arc::clone(&MANAGER_STORE);
		handle_request(req, mng_store).await
	});
	let res = https.serve_connection(stream, service_function).await;
	if let Err(err) = res {
		println!("Error serving connection: {:?}", err);
	}
}

fn handle_epoch_convergence(arc_manager: Arc<Mutex<Manager>>, epoch: Epoch) {
	let manager = arc_manager.lock();

	if manager.is_err() {
		let e = manager.err();
		println!("error: {:?}", e);
		return;
	}

	let mut manager = manager.unwrap();
	manager.calculate_initial_ivps(epoch);

	for i in 0..NUM_ITERATIONS {
		manager.calculate_ivps(epoch, i);
	}
}

#[tokio::main]
pub async fn main() -> Result<(), EigenError> {
	let addr: SocketAddr = ([127, 0, 0, 1], 3000).into();

	let listener = TcpListener::bind(addr).await.map_err(|_| EigenError::ListenError)?;
	println!("Listening on https://{}", addr);

	let interval = Duration::from_secs(EPOCH_INTERVAL);
	let mut inner_interval = time::interval(interval);
	inner_interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

	loop {
		select! {
			res = listener.accept() => {
				let (stream, addr) = res.map_err(|_| EigenError::ConnectionError)?;
				handle_connection(stream, addr).await;
			}
			_res = inner_interval.tick() => {
				let mng_store = Arc::clone(&MANAGER_STORE);
				let epoch = Epoch::current_epoch(EPOCH_INTERVAL);
				handle_epoch_convergence(mng_store, epoch);
			}
		};
	}
}

#[cfg(test)]
mod test {
	use crate::{
		constants::{NUM_BOOTSTRAP_PEERS, NUM_ITERATIONS},
		utils::{generate_pk_from_sk, scalar_from_bs58},
	};
	use eigen_trust_circuit::{
		halo2wrong::{
			curves::bn256::Bn256,
			halo2::{
				arithmetic::Field,
				poly::{commitment::ParamsProver, kzg::commitment::ParamsKZG},
			},
		},
		params::poseidon_bn254_5x5::Params,
		utils::{keygen, random_circuit},
	};
	use hyper::{body::HttpBody, Uri};
	use rand::thread_rng;
	use serde_json::to_vec;

	use super::*;

	const SK_KEY1: &str = "AF4yAqwCPzpBcit4FtTrHso4BBR9onk7qS9Q1SWSLSaV";
	const SK_KEY2: &str = "7VoQFngkSo36s5yzZtnjtZ5SLe1VGukCZdb5Uc9tSDNC";
	const SK_KEY3: &str = "3wEvtEFktXUBHZHPPmLkDh7oqFLnjTPep1EJ2eBqLtcX";

	#[tokio::test]
	async fn should_fail_without_query() {
		let mut rng = thread_rng();
		let params = ParamsKZG::<Bn256>::new(9);
		let random_circuit =
			random_circuit::<Bn256, _, MAX_NEIGHBORS, NUM_BOOTSTRAP_PEERS, Params>(&mut rng);
		let proving_key = keygen(&params, &random_circuit).unwrap();

		let manager = Manager::new(params, proving_key);

		let req = Request::get(Uri::from_static("http://localhost:3000/score"))
			.body(Body::default())
			.unwrap();

		let arc_manager = Arc::new(Mutex::new(manager));

		let res = handle_request(req, arc_manager).await.unwrap();
		assert_eq!(*res.body(), ResponseBody::InvalidQuery.to_string());
	}

	#[tokio::test]
	async fn should_fail_with_wrong_public_key() {
		let mut rng = thread_rng();
		let params = ParamsKZG::<Bn256>::new(9);
		let random_circuit =
			random_circuit::<Bn256, _, MAX_NEIGHBORS, NUM_BOOTSTRAP_PEERS, Params>(&mut rng);
		let proving_key = keygen(&params, &random_circuit).unwrap();

		let manager = Manager::new(params, proving_key);

		let req = Request::get(Uri::from_static(
			"http://localhost:3000/score?pk=abcd__123&epoch=123",
		))
		.body(Body::default())
		.unwrap();

		let arc_manager = Arc::new(Mutex::new(manager));

		let res = handle_request(req, arc_manager).await.unwrap();
		assert_eq!(*res.body(), ResponseBody::InvalidQuery.to_string());
	}

	#[tokio::test]
	async fn should_fail_with_wrong_epoch() {
		let mut rng = thread_rng();
		let params = ParamsKZG::<Bn256>::new(9);
		let random_circuit =
			random_circuit::<Bn256, _, MAX_NEIGHBORS, NUM_BOOTSTRAP_PEERS, Params>(&mut rng);
		let proving_key = keygen(&params, &random_circuit).unwrap();

		let manager = Manager::new(params, proving_key);

		let req = Request::get(Uri::from_static(
			"http://localhost:3000/score?pk=abcd123&epoch=abc",
		))
		.body(Body::default())
		.unwrap();

		let arc_manager = Arc::new(Mutex::new(manager));

		let res = handle_request(req, arc_manager).await.unwrap();
		assert_eq!(*res.body(), ResponseBody::InvalidQuery.to_string());
	}

	#[tokio::test]
	async fn should_fail_with_incomplete_query() {
		let mut rng = thread_rng();
		let params = ParamsKZG::<Bn256>::new(9);
		let random_circuit =
			random_circuit::<Bn256, _, MAX_NEIGHBORS, NUM_BOOTSTRAP_PEERS, Params>(&mut rng);
		let proving_key = keygen(&params, &random_circuit).unwrap();

		let manager = Manager::new(params, proving_key);

		let req = Request::get(Uri::from_static("http://localhost:3000/score?pk=abcd123"))
			.body(Body::default())
			.unwrap();

		let arc_manager = Arc::new(Mutex::new(manager));

		let res = handle_request(req, arc_manager).await.unwrap();
		assert_eq!(*res.body(), ResponseBody::InvalidQuery.to_string());
	}

	#[tokio::test]
	async fn should_query_score() {
		let mut rng = thread_rng();
		let params = ParamsKZG::<Bn256>::new(9);
		let random_circuit =
			random_circuit::<Bn256, _, MAX_NEIGHBORS, NUM_BOOTSTRAP_PEERS, Params>(&mut rng);
		let proving_key = keygen(&params, &random_circuit).unwrap();

		let epoch = Epoch(123);
		let mut manager = Manager::new(params, proving_key);

		let sk1 = scalar_from_bs58(SK_KEY1);
		let pk1 = generate_pk_from_sk(sk1);

		let sk2 = scalar_from_bs58(SK_KEY2);
		let pk2 = generate_pk_from_sk(sk2);

		let sk3 = scalar_from_bs58(SK_KEY3);
		let pk3 = generate_pk_from_sk(sk3);

		let mut neighbours1 = [None; MAX_NEIGHBORS];
		neighbours1[0] = Some(pk2);
		neighbours1[1] = Some(pk3);

		let mut neighbours2 = [None; MAX_NEIGHBORS];
		neighbours2[0] = Some(pk1);
		neighbours2[1] = Some(pk3);

		let mut neighbours3 = [None; MAX_NEIGHBORS];
		neighbours3[0] = Some(pk1);
		neighbours3[1] = Some(pk2);

		let mut scores1 = [None; MAX_NEIGHBORS];
		scores1[0] = Some(10.);
		scores1[1] = Some(20.);
		let mut scores2 = [None; MAX_NEIGHBORS];
		scores2[0] = Some(10.);
		scores2[1] = Some(20.);
		let mut scores3 = [None; MAX_NEIGHBORS];
		scores3[0] = Some(10.);
		scores3[1] = Some(20.);

		let sig1 = Signature::new(sk1, pk1, neighbours1, scores1);
		let sig2 = Signature::new(sk2, pk2, neighbours2, scores2);
		let sig3 = Signature::new(sk3, pk3, neighbours3, scores3);

		manager.add_signature(sig1);
		manager.add_signature(sig2);
		manager.add_signature(sig3);

		manager.calculate_initial_ivps(epoch);

		for i in 0..NUM_ITERATIONS {
			manager.calculate_ivps(epoch, i);
		}

		let req = Request::get(Uri::from_static(
			"http://localhost:3000/score?pk=52RwQpZ9kUDsNi9R8f5FMD27pqyTPB39hQKYeH7fH99P&epoch=123",
		))
		.body(Body::default())
		.unwrap();

		let arc_manager = Arc::new(Mutex::new(manager));

		let res = handle_request(req, arc_manager).await.unwrap();
		assert_eq!(*res.body(), "0.3749428495839048");
	}

	#[tokio::test]
	async fn should_fail_signature_add_with_invalid_data() {
		let mut rng = thread_rng();
		let params = ParamsKZG::<Bn256>::new(9);
		let random_circuit =
			random_circuit::<Bn256, _, MAX_NEIGHBORS, NUM_BOOTSTRAP_PEERS, Params>(&mut rng);
		let proving_key = keygen(&params, &random_circuit).unwrap();

		let manager = Manager::new(params, proving_key);

		let sk = scalar_from_bs58(SK_KEY1);
		let pk = generate_pk_from_sk(sk);
		let neighbours = [None; MAX_NEIGHBORS];
		let scores = [None; MAX_NEIGHBORS];
		let signature = Signature::new(sk, pk, neighbours, scores);
		let signature_data: SignatureData = signature.into();
		let mut signature_bytes = to_vec(&signature_data).unwrap();
		// Remove some bytes
		signature_bytes.drain(..10);

		let req = Request::post(Uri::from_static("http://localhost:3000/signature"))
			.body(Body::from(signature_bytes))
			.unwrap();

		let arc_manager = Arc::new(Mutex::new(manager));

		let res = handle_request(req, arc_manager).await.unwrap();
		assert_eq!(*res.body(), ResponseBody::InvalidRequest.to_string());
	}

	#[tokio::test]
	async fn should_add_signature() {
		let mut rng = thread_rng();
		let params = ParamsKZG::<Bn256>::new(9);
		let random_circuit =
			random_circuit::<Bn256, _, MAX_NEIGHBORS, NUM_BOOTSTRAP_PEERS, Params>(&mut rng);
		let proving_key = keygen(&params, &random_circuit).unwrap();

		let manager = Manager::new(params, proving_key);

		let sk = scalar_from_bs58(SK_KEY1);
		let pk = generate_pk_from_sk(sk);
		let neighbours = [None; MAX_NEIGHBORS];
		let scores = [None; MAX_NEIGHBORS];
		let signature = Signature::new(sk, pk, neighbours, scores);
		let signature_data: SignatureData = signature.into();
		let signature_bytes = to_vec(&signature_data).unwrap();

		let req = Request::post(Uri::from_static("http://localhost:3000/signature"))
			.body(Body::from(signature_bytes))
			.unwrap();

		let arc_manager = Arc::new(Mutex::new(manager));

		let res = handle_request(req, arc_manager).await.unwrap();
		assert_eq!(*res.body(), ResponseBody::SignatureAddSuccess.to_string());
	}

	#[tokio::test]
	async fn should_fail_if_route_is_not_found() {
		let mut rng = thread_rng();
		let params = ParamsKZG::<Bn256>::new(9);
		let random_circuit =
			random_circuit::<Bn256, _, MAX_NEIGHBORS, NUM_BOOTSTRAP_PEERS, Params>(&mut rng);
		let proving_key = keygen(&params, &random_circuit).unwrap();

		let manager = Manager::new(params, proving_key);

		let req = Request::get(Uri::from_static("http://localhost:3000/non_existing_route"))
			.body(Body::default())
			.unwrap();

		let arc_manager = Arc::new(Mutex::new(manager));

		let res = handle_request(req, arc_manager).await.unwrap();
		assert_eq!(*res.body(), ResponseBody::InvalidRequest.to_string());
	}

	#[test]
	fn should_run_one_epoch() {
		let mut rng = thread_rng();
		let params = ParamsKZG::<Bn256>::new(9);
		let random_circuit =
			random_circuit::<Bn256, _, MAX_NEIGHBORS, NUM_BOOTSTRAP_PEERS, Params>(&mut rng);
		let proving_key = keygen(&params, &random_circuit).unwrap();

		let epoch = Epoch::current_epoch(EPOCH_INTERVAL);
		let mut manager = Manager::new(params, proving_key);

		let sk1 = scalar_from_bs58(SK_KEY1);
		let pk1 = generate_pk_from_sk(sk1);

		let sk2 = scalar_from_bs58(SK_KEY2);
		let pk2 = generate_pk_from_sk(sk2);

		let sk3 = scalar_from_bs58(SK_KEY3);
		let pk3 = generate_pk_from_sk(sk3);

		let mut neighbours1 = [None; MAX_NEIGHBORS];
		neighbours1[0] = Some(pk2);
		neighbours1[1] = Some(pk3);

		let mut neighbours2 = [None; MAX_NEIGHBORS];
		neighbours2[0] = Some(pk1);
		neighbours2[1] = Some(pk3);

		let mut neighbours3 = [None; MAX_NEIGHBORS];
		neighbours3[0] = Some(pk1);
		neighbours3[1] = Some(pk2);

		let mut scores1 = [None; MAX_NEIGHBORS];
		scores1[0] = Some(10.);
		scores1[1] = Some(20.);
		let mut scores2 = [None; MAX_NEIGHBORS];
		scores2[0] = Some(10.);
		scores2[1] = Some(20.);
		let mut scores3 = [None; MAX_NEIGHBORS];
		scores3[0] = Some(10.);
		scores3[1] = Some(20.);

		let sig1 = Signature::new(sk1, pk1, neighbours1, scores1);
		let sig2 = Signature::new(sk2, pk2, neighbours2, scores2);
		let sig3 = Signature::new(sk3, pk3, neighbours3, scores3);

		manager.add_signature(sig1.clone());
		manager.add_signature(sig2);
		manager.add_signature(sig3);

		let arc_manager = Arc::new(Mutex::new(manager));

		handle_epoch_convergence(Arc::clone(&arc_manager), epoch);

		let mng = arc_manager.lock().unwrap();
		let op_ji = mng.get_op_jis(&sig1, epoch, NUM_ITERATIONS);
		assert_eq!(op_ji.iter().sum::<f64>(), 0.3749428495839048);
	}

	#[tokio::test]
	async fn should_complete_request_flow() {
		let mut rng = thread_rng();
		let params = ParamsKZG::<Bn256>::new(9);
		let random_circuit =
			random_circuit::<Bn256, _, MAX_NEIGHBORS, NUM_BOOTSTRAP_PEERS, Params>(&mut rng);
		let proving_key = keygen(&params, &random_circuit).unwrap();

		let epoch = Epoch(123);
		let manager = Manager::new(params, proving_key);

		let sk1 = scalar_from_bs58(SK_KEY1);
		let pk1 = generate_pk_from_sk(sk1);

		let sk2 = scalar_from_bs58(SK_KEY2);
		let pk2 = generate_pk_from_sk(sk2);

		let sk3 = scalar_from_bs58(SK_KEY3);
		let pk3 = generate_pk_from_sk(sk3);

		let mut neighbours1 = [None; MAX_NEIGHBORS];
		neighbours1[0] = Some(pk2);
		neighbours1[1] = Some(pk3);

		let mut neighbours2 = [None; MAX_NEIGHBORS];
		neighbours2[0] = Some(pk1);
		neighbours2[1] = Some(pk3);

		let mut neighbours3 = [None; MAX_NEIGHBORS];
		neighbours3[0] = Some(pk1);
		neighbours3[1] = Some(pk2);

		let mut scores1 = [None; MAX_NEIGHBORS];
		scores1[0] = Some(10.);
		scores1[1] = Some(20.);
		let mut scores2 = [None; MAX_NEIGHBORS];
		scores2[0] = Some(10.);
		scores2[1] = Some(20.);
		let mut scores3 = [None; MAX_NEIGHBORS];
		scores3[0] = Some(10.);
		scores3[1] = Some(20.);

		let sig1 = Signature::new(sk1, pk1, neighbours1, scores1);
		let sig2 = Signature::new(sk2, pk2, neighbours2, scores2);
		let sig3 = Signature::new(sk3, pk3, neighbours3, scores3);

		let sig_data1: SignatureData = sig1.into();
		let sig_data2: SignatureData = sig2.into();
		let sig_data3: SignatureData = sig3.into();

		let sig_bytes1 = to_vec(&sig_data1).unwrap();
		let sig_bytes2 = to_vec(&sig_data2).unwrap();
		let sig_bytes3 = to_vec(&sig_data3).unwrap();

		let req1 = Request::post(Uri::from_static("http://localhost:3000/signature"))
			.body(Body::from(sig_bytes1))
			.unwrap();
		let req2 = Request::post(Uri::from_static("http://localhost:3000/signature"))
			.body(Body::from(sig_bytes2))
			.unwrap();
		let req3 = Request::post(Uri::from_static("http://localhost:3000/signature"))
			.body(Body::from(sig_bytes3))
			.unwrap();

		let arc_manager = Arc::new(Mutex::new(manager));
		let res1 = handle_request(req1, Arc::clone(&arc_manager)).await.unwrap();
		let res2 = handle_request(req2, Arc::clone(&arc_manager)).await.unwrap();
		let res3 = handle_request(req3, Arc::clone(&arc_manager)).await.unwrap();

		assert_eq!(*res1.body(), ResponseBody::SignatureAddSuccess.to_string());
		assert_eq!(*res2.body(), ResponseBody::SignatureAddSuccess.to_string());
		assert_eq!(*res3.body(), ResponseBody::SignatureAddSuccess.to_string());

		handle_epoch_convergence(Arc::clone(&arc_manager), epoch);

		let req4 = Request::get(Uri::from_static(
			"http://localhost:3000/score?pk=52RwQpZ9kUDsNi9R8f5FMD27pqyTPB39hQKYeH7fH99P&epoch=123",
		))
		.body(Body::default())
		.unwrap();

		let res4 = handle_request(req4, Arc::clone(&arc_manager)).await.unwrap();
		assert_eq!(*res4.body(), "0.3749428495839048");
	}

	#[tokio::test]
	async fn non_existing_neighbours() {
		let mut rng = thread_rng();
		let params = ParamsKZG::<Bn256>::new(9);
		let random_circuit =
			random_circuit::<Bn256, _, MAX_NEIGHBORS, NUM_BOOTSTRAP_PEERS, Params>(&mut rng);
		let proving_key = keygen(&params, &random_circuit).unwrap();

		let epoch = Epoch(123);
		let manager = Manager::new(params, proving_key);

		let sk1 = scalar_from_bs58(SK_KEY1);
		let pk1 = generate_pk_from_sk(sk1);

		let sk2 = scalar_from_bs58(SK_KEY2);
		let pk2 = generate_pk_from_sk(sk2);

		let sk3 = scalar_from_bs58(SK_KEY3);
		let pk3 = generate_pk_from_sk(sk3);

		let mut neighbours1 = [None; MAX_NEIGHBORS];
		neighbours1[0] = Some(pk2);
		neighbours1[1] = Some(pk3);

		let mut scores1 = [None; MAX_NEIGHBORS];
		scores1[0] = Some(10.);
		scores1[1] = Some(20.);

		let sig1 = Signature::new(sk1, pk1, neighbours1, scores1);

		let sig_data1: SignatureData = sig1.into();

		let sig_bytes1 = to_vec(&sig_data1).unwrap();

		let req1 = Request::post(Uri::from_static("http://localhost:3000/signature"))
			.body(Body::from(sig_bytes1))
			.unwrap();

		let arc_manager = Arc::new(Mutex::new(manager));
		let res1 = handle_request(req1, Arc::clone(&arc_manager)).await.unwrap();

		assert_eq!(*res1.body(), ResponseBody::SignatureAddSuccess.to_string());

		handle_epoch_convergence(Arc::clone(&arc_manager), epoch);

		let req4 = Request::get(Uri::from_static(
			"http://localhost:3000/score?pk=HhfwhxzwKvS8UGVvfnyJUiA1uL1VhXXfqFWh4BtEM9zx&epoch=123",
		))
		.body(Body::default())
		.unwrap();

		let res4 = handle_request(req4, Arc::clone(&arc_manager)).await.unwrap();
		assert_eq!(*res4.body(), ResponseBody::InvalidQuery.to_string());
	}
}
