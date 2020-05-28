// Copyright 2020 Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

//! The paras module is responsible for storing data on parachains and parathreads.
//!
//! It tracks which paras are parachains, what their current head data is in
//! this fork of the relay chain, what their validation code is, and what their past and upcoming
//! validation code is.
//!
//! A para is not considered live until it is registered and activated in this module. Activation can
//! only occur at session boundaries.

use sp_std::prelude::*;
use sp_std::marker::PhantomData;
use sp_runtime::traits::One;
use primitives::{
	parachain::{ValidatorId, Id as ParaId, ValidationCode, HeadData},
};
use frame_support::{
	decl_storage, decl_module, decl_error,
	dispatch::DispatchResult,
	traits::Get,
	weights::{DispatchClass, Weight, constants::{WEIGHT_PER_SECOND}},
};
use codec::{Encode, Decode};
use system::ensure_root;
use crate::configuration;

#[cfg(feature = "std")]
use serde::{Serialize, Deserialize};

pub trait Trait: system::Trait + configuration::Trait { }

/// Metadata used to track previous parachain validation code that we keep in
/// the state.
#[derive(Default, Encode, Decode)]
#[cfg_attr(test, derive(Debug, Clone, PartialEq))]
pub struct ParaPastCodeMeta<N> {
	// Block numbers where the code was "technically" replaced and the block number at
	// which the code was actually replaced. These can be used as indices
	// into the `PastCode` map along with the `ParaId` to fetch the code itself.
	upgrade_times: Vec<(N, N)>,
	// This tracks the highest pruned code-replacement, if any.
	last_pruned: Option<N>,
}

#[cfg_attr(test, derive(Debug, PartialEq))]
enum UseCodeAt<N> {
	// Use the current code.
	Current,
	// Use the code that was replaced at the given block number.
	ReplacedAt(N),
}

impl<N: Ord + Copy> ParaPastCodeMeta<N> {
	// note a replacement has occurred at a given block number.
	fn note_replacement(&mut self, at: N, included_at: N) {
		self.upgrade_times.insert(0, (at, included_at))
	}

	// Yields the block number of the code that should be used for validating at
	// the given block number.
	//
	// a return value of `None` means that there is no code we are aware of that
	// should be used to validate at the given height.
	fn code_at(&self, at: N) -> Option<UseCodeAt<N>> {
		// The `PastCode` map stores the code which was replaced at `t`.
		let end_position = self.upgrade_times.iter().position(|&t| t.0 < at);
		if let Some(end_position) = end_position {
			Some(if end_position != 0 {
				// `end_position` gives us the replacement time where the code used at `at`
				// was set. But that code has been replaced: `end_position - 1` yields
				// that index.
				UseCodeAt::ReplacedAt(self.upgrade_times[end_position - 1].0)
			} else {
				// the most recent tracked replacement is before `at`.
				// this means that the code put in place then (i.e. the current code)
				// is correct for validating at `at`.
				UseCodeAt::Current
			})
		} else {
			if self.last_pruned.as_ref().map_or(true, |&n| n < at) {
				// Our `last_pruned` is before `at`, so we still have the code!
				// but no code upgrade entries found before the `at` parameter.
				//
				// this means one of two things is true:
				// 1. there are no non-pruned upgrade logs. in this case use `Current`
				// 2. there are non-pruned upgrade logs all after `at`.
				//    in this case use the oldest upgrade log.
				Some(self.upgrade_times.last()
					.map(|n| UseCodeAt::ReplacedAt(n.0))
					.unwrap_or(UseCodeAt::Current)
				)
			} else {
				// We don't have the code anymore.
				None
			}
		}
	}

	// The block at which the most recently tracked code change occurred, from the perspective
	// of the para.
	fn most_recent_change(&self) -> Option<N> {
		self.upgrade_times.first().map(|x| x.0.clone())
	}

	// prunes all code upgrade logs occurring at or before `max`.
	// note that code replaced at `x` is the code used to validate all blocks before
	// `x`. Thus, `max` should be outside of the slashing window when this is invoked.
	//
	// returns an iterator of block numbers at which code was replaced, where the replaced
	// code should be now pruned, in ascending order.
	fn prune_up_to(&'_ mut self, max: N) -> impl Iterator<Item=N> + '_ {
		let drained = match self.upgrade_times.iter().position(|&t| t.1 <= max) {
			None => {
				// this is a no-op `drain` - desired because all
				// logged code upgrades occurred after `max`.
				self.upgrade_times.drain(self.upgrade_times.len()..).rev()
			}
			Some(pos) => {
				self.last_pruned = Some(self.upgrade_times[pos].0);
				self.upgrade_times.drain(pos..).rev()
			}
		};

		drained.map(|(replaced, _)| replaced)
	}
}

/// Arguments for initializing a para.
#[derive(Encode, Decode)]
#[cfg_attr(feature = "std", derive(Serialize, Deserialize))]
pub struct ParaGenesisArgs {
	/// The initial head data to use.
	genesis_head: HeadData,
	/// The initial validation code to use.
	validation_code: ValidationCode,
	/// True if parachain, false if parathread.
	parachain: bool,
}


decl_storage! {
	trait Store for Module<T: Trait> as Paras {
		/// All parachains. Ordered ascending by ParaId. Parathreads are not included.
		Parachains get(fn parachains): Vec<ParaId>;
		/// The head-data of every registered para.
		Heads get(fn parachain_head): map hasher(twox_64_concat) ParaId => Option<HeadData>;
		/// The validation code of every live para.
		CurrentCode get(fn current_code): map hasher(twox_64_concat) ParaId => Option<ValidationCode>;
		/// Actual past code, indicated by the para id as well as the block number at which it became outdated.
		PastCode: map hasher(twox_64_concat) (ParaId, T::BlockNumber) => Option<ValidationCode>;
		/// Past code of parachains. The parachains themselves may not be registered anymore,
		/// but we also keep their code on-chain for the same amount of time as outdated code
		/// to keep it available for secondary checkers.
		PastCodeMeta get(fn past_code_meta):
			map hasher(twox_64_concat) ParaId => ParaPastCodeMeta<T::BlockNumber>;
		/// Which paras have past code that needs pruning and the relay-chain block at which the code was replaced.
		/// Note that this is the actual height of the included block, not the expected height at which the
		/// code upgrade would be applied, although they may be equal.
		/// This is to ensure the entire acceptance period is covered, not an offset acceptance period starting
		/// from the time at which the parachain perceives a code upgrade as having occurred.
		/// Multiple entries for a single para are permitted. Ordered ascending by block number.
		PastCodePruning: Vec<(ParaId, T::BlockNumber)>;
		/// The block number at which the planned code change is expected for a para.
		/// The change will be applied after the first parablock for this ID included which executes
		/// in the context of a relay chain block with a number >= `expected_at`.
		FutureCodeUpgrades get(fn future_code_upgrade_at): map hasher(twox_64_concat) ParaId => Option<T::BlockNumber>;
		/// The actual future code of a para.
		FutureCode: map hasher(twox_64_concat) ParaId => ValidationCode;

		/// Upcoming paras (chains and threads). These are only updated on session change. Corresponds to an
		/// entry in the upcoming-genesis map.
		UpcomingParas: Vec<ParaId>;
		/// Upcoming paras instantiation arguments.
		UpcomingParasGenesis: map hasher(twox_64_concat) ParaId => Option<ParaGenesisArgs>;
		/// Paras that are to be cleaned up at the end of the session.
		OutgoingParas: Vec<ParaId>;

	}
	add_extra_genesis {
		config(paras): Vec<(ParaId, ParaGenesisArgs)>;
		config(_phdata): PhantomData<T>;
		build(build::<T>);
	}
}

#[cfg(feature = "std")]
fn build<T: Trait>(config: &GenesisConfig<T>) {
		.iter()
		.filter(|(_, args)| args.parachain)
		.map(|&(ref id, _)| id)
		.cloned()
		.collect();

	parachains.sort_unstable();
	parachains.dedup();

	Parachains::put(&parachains);

	for (id, genesis_args) in &config.paras {
		println!("Initializing genesis for para {:?}", id);
		<Module<T> as Store>::CurrentCode::insert(&id, &genesis_args.validation_code);
		<Module<T> as Store>::Heads::insert(&id, &genesis_args.genesis_head);
	}
}

decl_error! {
	pub enum Error for Module<T: Trait> { }
}

decl_module! {
	/// The parachains configuration module.
	pub struct Module<T: Trait> for enum Call where origin: <T as system::Trait>::Origin {
		type Error = Error<T>;
	}
}

impl<T: Trait> Module<T> {
	/// Called by the initializer to initialize the configuration module.
	pub(crate) fn initializer_initialize(now: T::BlockNumber) -> Weight {
		Self::do_old_code_pruning(now)
	}

	/// Called by the initializer to finalize the configuration module.
	pub(crate) fn initializer_finalize() { }

	/// Called by the initializer to note that a new session has started.
	pub(crate) fn initializer_on_new_session(_validators: &[ValidatorId], _queued: &[ValidatorId]) {
		let now = <system::Module<T>>::block_number();
		let mut parachains = Self::clean_up_outgoing(now);
		Self::apply_incoming(&mut parachains);
		<Self as Store>::Parachains::set(parachains);
	}

	/// Cleans up all outgoing paras. Returns the new set of parachains
	fn clean_up_outgoing(now: T::BlockNumber) -> Vec<ParaId> {
		let mut parachains = <Self as Store>::Parachains::get();
		let outgoing = <Self as Store>::OutgoingParas::take();

		for outgoing_para in outgoing {
			if let Ok(i) = parachains.binary_search(&outgoing_para) {
				parachains.remove(i);
			}

			<Self as Store>::Heads::remove(&outgoing_para);
			<Self as Store>::FutureCodeUpgrades::remove(&outgoing_para);
			<Self as Store>::FutureCode::remove(&outgoing_para);

			let removed_code = <Self as Store>::CurrentCode::take(&outgoing_para);
			if let Some(removed_code) = removed_code {
				Self::note_past_code(outgoing_para, now, now, removed_code);
			}
		}

		parachains
	}

	/// Applies all incoming paras, updating the parachains list for those that are parachains.
	fn apply_incoming(parachains: &mut Vec<ParaId>) {
		let upcoming = <Self as Store>::UpcomingParas::take();
		for upcoming_para in upcoming {
			let genesis_data = match <Self as Store>::UpcomingParasGenesis::take(&upcoming_para) {
				None => continue,
				Some(g) => g,
			};

			if genesis_data.parachain {
				match parachains.binary_search(&upcoming_para) {
					Ok(_i) => {}
					Err(i) => {
						parachains.insert(i, upcoming_para);
					}
				}
			}

			<Self as Store>::Heads::insert(&upcoming_para, genesis_data.genesis_head);
			<Self as Store>::CurrentCode::insert(&upcoming_para, genesis_data.validation_code);
		}
	}

	// note replacement of the code of para with given `id`, which occured in the
	// context of the given relay-chain block number. provide the replaced code.
	//
	// `at` for para-triggered replacement is the block number of the relay-chain
	// block in whose context the parablock was executed
	// (i.e. number of `relay_parent` in the receipt)
	fn note_past_code(
		id: ParaId,
		at: T::BlockNumber,
		now: T::BlockNumber,
		old_code: ValidationCode,
	) -> Weight {

		<Self as Store>::PastCodeMeta::mutate(&id, |past_meta| {
			past_meta.note_replacement(at, now);
		});

		<Self as Store>::PastCode::insert(&(id, at), old_code);

		// Schedule pruning for this past-code to be removed as soon as it
		// exits the slashing window.
		<Self as Store>::PastCodePruning::mutate(|pruning| {
			let insert_idx = pruning.binary_search_by_key(&at, |&(_, b)| b)
				.unwrap_or_else(|idx| idx);
			pruning.insert(insert_idx, (id, now));
		});

		T::DbWeight::get().reads_writes(2, 3)
	}

	// does old code pruning.
	fn do_old_code_pruning(now: T::BlockNumber) -> Weight {
		let config = configuration::Module::<T>::config();
		let acceptance_period = config.acceptance_period;
		if now <= acceptance_period {
			let weight = T::DbWeight::get().reads_writes(1, 0);
			return weight;
		}

		// The height of any changes we no longer should keep around.
		let pruning_height = now - (acceptance_period + One::one());

		let pruning_tasks_done =
			<Self as Store>::PastCodePruning::mutate(|pruning_tasks: &mut Vec<(_, T::BlockNumber)>| {
				let (pruning_tasks_done, pruning_tasks_to_do) = {
					// find all past code that has just exited the pruning window.
					let up_to_idx = pruning_tasks.iter()
						.take_while(|&(_, at)| at <= &pruning_height)
						.count();
					(up_to_idx, pruning_tasks.drain(..up_to_idx))
				};

				for (para_id, _) in pruning_tasks_to_do {
					let full_deactivate = <Self as Store>::PastCodeMeta::mutate(&para_id, |meta| {
						for pruned_repl_at in meta.prune_up_to(pruning_height) {
							<Self as Store>::PastCode::remove(&(para_id, pruned_repl_at));
						}

						meta.most_recent_change().is_none() && Self::parachain_head(&para_id).is_none()
					});

					// This parachain has been removed and now the vestigial code
					// has been removed from the state. clean up meta as well.
					if full_deactivate {
						<Self as Store>::PastCodeMeta::remove(&para_id);
					}
				}

				pruning_tasks_done as u64
			});

		// 1 read for the meta for each pruning task, 1 read for the config
		// 2 writes: updating the meta and pruning the code
		T::DbWeight::get().reads_writes(1 + pruning_tasks_done, 2 * pruning_tasks_done)
	}

	/// Schedule a para to be initialized at the start of the next session.
	pub(crate) fn schedule_para_initialize(id: ParaId, genesis: ParaGenesisArgs) -> Weight {
		let dup = UpcomingParas::mutate(|v| {
			match v.binary_search(&id) {
				Ok(_) => true,
				Err(i) => {
					v.insert(i, id);
					false
				}
			}
		});

		if dup {
			let weight = T::DbWeight::get().reads_writes(1, 0);
			return weight;
		}

		UpcomingParasGenesis::insert(&id, &genesis);

		T::DbWeight::get().reads_writes(1, 2)
	}

	/// Schedule a para to be cleaned up at the start of the next session.
	pub(crate) fn schedule_para_cleanup(id: ParaId) -> Weight {
		OutgoingParas::mutate(|v| {
			match v.binary_search(&id) {
				Ok(_) => T::DbWeight::get().reads_writes(1, 0),
				Err(i) => {
					v.insert(i, id);
					T::DbWeight::get().reads_writes(1, 1)
				}
			}
		})
	}

	/// Schedule a future code upgrade of the given parachain, to be applied after inclusion
	/// of a block of the same parachain executed in the context of a relay-chain block
	/// with number >= `expected_at`
	///
	/// If there is already a scheduled code upgrade for the para, this is a no-op.
	pub(crate) fn schedule_code_upgrade(
		id: ParaId,
		new_code: ValidationCode,
		expected_at: T::BlockNumber,
	) -> Weight {
		<Self as Store>::FutureCodeUpgrades::mutate(&id, |up| {
			if up.is_some() {
				T::DbWeight::get().reads_writes(1, 0)
			} else {
				*up = Some(expected_at);
				FutureCode::insert(&id, new_code);
				T::DbWeight::get().reads_writes(1, 2)
			}
		})
	}

	/// Note that a para has progressed to a new head, where the new head was executed in the context
	/// of a relay-chain block with given number. This will apply pending code upgrades based
	/// on the block number provided.
	pub(crate) fn note_new_head(
		id: ParaId,
		new_head: HeadData,
		execution_context: T::BlockNumber,
	) -> Weight {
		if let Some(expected_at) = <Self as Store>::FutureCodeUpgrades::get(&id) {
			Heads::insert(&id, new_head);

			if expected_at <= execution_context {
				<Self as Store>::FutureCodeUpgrades::remove(&id);
				let new_code = FutureCode::take(&id);

				let prior_code = CurrentCode::get(&id).unwrap_or_default();
				CurrentCode::insert(&id, &new_code);

				let now = <system::Module<T>>::block_number();

				let weight = Self::note_past_code(
					id,
					expected_at,
					now,
					prior_code,
				);

				// add 1 to writes due to heads update.
				weight + T::DbWeight::get().reads_writes(3, 1 + 3)
			} else {
				T::DbWeight::get().reads_writes(1, 1 + 0)
			}
		} else {
			T::DbWeight::get().reads_writes(1, 0)
		}
	}

	/// Fetches the validation code to be used when validating a block in the context of the given
	/// relay-chain height. A second block number parameter may be used to tell the lookup to proceed
	/// as if an intermediate parablock has been with the given relay-chain height as its context.
	/// This may return past, current, or (with certain choices of `assume_intermediate`) future code.
	///
	/// `assume_intermediate`, if provided, must be before `at`. If `at` is not within the acceptance
	/// of the current block number, this will return `None`
	pub(crate) fn validation_code_at(
		id: ParaId,
		at: T::BlockNumber,
		assume_intermediate: Option<T::BlockNumber>,
	) -> Option<ValidationCode> {
		let now = <system::Module<T>>::block_number();
		let config = <configuration::Module<T>>::config();

		if assume_intermediate.as_ref().map_or(false, |i| &at <= i) {
			return None;
		}

		if at + config.acceptance_period + One::one() < now {
			return None;
		}

		let planned_upgrade = <Self as Store>::FutureCodeUpgrades::get(&id);
		let upgrade_applied_intermediate = match assume_intermediate {
			Some(a) => planned_upgrade.as_ref().map_or(false, |u| u <= &a),
			None => false,
		};

		if upgrade_applied_intermediate {
			Some(FutureCode::get(&id))
		} else {
			match Self::past_code_meta(&id).code_at(at) {
				None => None,
				Some(UseCodeAt::Current) => CurrentCode::get(&id),
				Some(UseCodeAt::ReplacedAt(replaced)) => <Self as Store>::PastCode::get(&(id, replaced))
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use primitives::BlockNumber;
	use frame_support::traits::{OnFinalize, OnInitialize};

	use crate::mock::{new_test_ext, Configuration, Paras, System, GenesisConfig as MockGenesisConfig};
	use crate::configuration::HostConfiguration;

	fn run_to_block(to: BlockNumber, new_session: Option<Vec<BlockNumber>>) {
		while System::block_number() < to {
			let b = System::block_number();
			Paras::initializer_finalize();
			System::on_finalize(b);

			System::on_initialize(b + 1);
			System::set_block_number(b + 1);

			if new_session.as_ref().map_or(false, |v| v.contains(&(b + 1))) {
				Paras::initializer_on_new_session(&[], &[]);
			}
			Paras::initializer_initialize(b + 1);
		}
	}

	#[test]
	fn para_past_code_meta_gives_right_code() {
		let mut past_code = ParaPastCodeMeta::default();
		assert_eq!(past_code.code_at(0u32), Some(UseCodeAt::Current));

		past_code.note_replacement(10, 12);
		assert_eq!(past_code.code_at(0), Some(UseCodeAt::ReplacedAt(10)));
		assert_eq!(past_code.code_at(10), Some(UseCodeAt::ReplacedAt(10)));
		assert_eq!(past_code.code_at(11), Some(UseCodeAt::Current));

		past_code.note_replacement(20, 25);
		assert_eq!(past_code.code_at(1), Some(UseCodeAt::ReplacedAt(10)));
		assert_eq!(past_code.code_at(10), Some(UseCodeAt::ReplacedAt(10)));
		assert_eq!(past_code.code_at(11), Some(UseCodeAt::ReplacedAt(20)));
		assert_eq!(past_code.code_at(20), Some(UseCodeAt::ReplacedAt(20)));
		assert_eq!(past_code.code_at(21), Some(UseCodeAt::Current));

		past_code.last_pruned = Some(5);
		assert_eq!(past_code.code_at(1), None);
		assert_eq!(past_code.code_at(5), None);
		assert_eq!(past_code.code_at(6), Some(UseCodeAt::ReplacedAt(10)));
	}

	#[test]
	fn para_past_code_pruning_works_correctly() {
		let mut past_code = ParaPastCodeMeta::default();
		past_code.note_replacement(10u32, 10);
		past_code.note_replacement(20, 25);
		past_code.note_replacement(30, 35);

		let old = past_code.clone();
		assert!(past_code.prune_up_to(9).collect::<Vec<_>>().is_empty());
		assert_eq!(old, past_code);

		assert_eq!(past_code.prune_up_to(10).collect::<Vec<_>>(), vec![10]);
		assert_eq!(past_code, ParaPastCodeMeta {
			upgrade_times: vec![(30, 35), (20, 25)],
			last_pruned: Some(10),
		});

		assert!(past_code.prune_up_to(21).collect::<Vec<_>>().is_empty());

		assert_eq!(past_code.prune_up_to(26).collect::<Vec<_>>(), vec![20]);
		assert_eq!(past_code, ParaPastCodeMeta {
			upgrade_times: vec![(30, 35)],
			last_pruned: Some(20),
		});

		past_code.note_replacement(40, 42);
		past_code.note_replacement(50, 53);
		past_code.note_replacement(60, 66);

		assert_eq!(past_code, ParaPastCodeMeta {
			upgrade_times: vec![(60, 66), (50, 53), (40, 42), (30, 35)],
			last_pruned: Some(20),
		});

		assert_eq!(past_code.prune_up_to(60).collect::<Vec<_>>(), vec![30, 40, 50]);
		assert_eq!(past_code, ParaPastCodeMeta {
			upgrade_times: vec![(60, 66)],
			last_pruned: Some(50),
		});

		assert_eq!(past_code.prune_up_to(66).collect::<Vec<_>>(), vec![60]);

		assert_eq!(past_code, ParaPastCodeMeta {
			upgrade_times: Vec::new(),
			last_pruned: Some(60),
		});
	}

	#[test]
	fn para_past_code_pruning_in_initialize() {
		let acceptance_period = 10;
		let paras = vec![
			(0u32.into(), ParaGenesisArgs {
				parachain: true,
				genesis_head: Default::default(),
				validation_code: Default::default(),
			}),
			(1u32.into(), ParaGenesisArgs {
				parachain: false,
				genesis_head: Default::default(),
				validation_code: Default::default(),
			}),
		];

		let genesis_config = MockGenesisConfig {
			paras: GenesisConfig { paras, ..Default::default() },
			configuration: crate::configuration::GenesisConfig {
				config: HostConfiguration {
					acceptance_period,
					..Default::default()
				},
				..Default::default()
			},
			..Default::default()
		};

		new_test_ext(genesis_config).execute_with(|| {
			let id = ParaId::from(0u32);
			let at_block: BlockNumber = 10;
			let included_block: BlockNumber = 12;

			<Paras as Store>::PastCode::insert(&(id, at_block), &ValidationCode(vec![1, 2, 3]));
			<Paras as Store>::PastCodePruning::put(&vec![(id, included_block)]);

			{
				let mut code_meta = Paras::past_code_meta(&id);
				code_meta.note_replacement(at_block, included_block);
				<Paras as Store>::PastCodeMeta::insert(&id, &code_meta);
			}

			let pruned_at: BlockNumber = included_block + acceptance_period + 1;
			assert_eq!(<Paras as Store>::PastCode::get(&(id, at_block)), Some(vec![1, 2, 3].into()));

			run_to_block(pruned_at - 1, None);
			assert_eq!(<Paras as Store>::PastCode::get(&(id, at_block)), Some(vec![1, 2, 3].into()));
			assert_eq!(Paras::past_code_meta(&id).most_recent_change(), Some(at_block));

			run_to_block(pruned_at, None);
			assert!(<Paras as Store>::PastCode::get(&(id, at_block)).is_none());
			assert!(Paras::past_code_meta(&id).most_recent_change().is_none());
		});
	}

	#[test]
	fn note_past_code_sets_up_pruning_correctly() {
		let acceptance_period = 10;
		let paras = vec![
			(0u32.into(), ParaGenesisArgs {
				parachain: true,
				genesis_head: Default::default(),
				validation_code: Default::default(),
			}),
			(1u32.into(), ParaGenesisArgs {
				parachain: false,
				genesis_head: Default::default(),
				validation_code: Default::default(),
			}),
		];

		let genesis_config = MockGenesisConfig {
			paras: GenesisConfig { paras, ..Default::default() },
			configuration: crate::configuration::GenesisConfig {
				config: HostConfiguration {
					acceptance_period,
					..Default::default()
				},
				..Default::default()
			},
			..Default::default()
		};

		new_test_ext(genesis_config).execute_with(|| {
			let id_a = ParaId::from(0u32);
			let id_b = ParaId::from(1u32);

			Paras::note_past_code(id_a, 10, 12, vec![1, 2, 3].into());
			Paras::note_past_code(id_b, 20, 23, vec![4, 5, 6].into());

			assert_eq!(<Paras as Store>::PastCodePruning::get(), vec![(id_a, 10), (id_b, 20)]);
			assert_eq!(
				Paras::past_code_meta(&id_a),
				ParaPastCodeMeta {
					upgrade_times: vec![(10, 12)],
					last_pruned: None,
				}
			);
			assert_eq!(
				Paras::past_code_meta(&id_b),
				ParaPastCodeMeta {
					upgrade_times: vec![(20, 23)],
					last_pruned: None,
				}
			);
		});
	}

	#[test]
	fn code_upgrade_applied_after_delay() {
		let acceptance_period = 10;
		let validation_upgrade_delay = 5;

		let paras = vec![
			(0u32.into(), ParaGenesisArgs {
				parachain: true,
				genesis_head: Default::default(),
				validation_code: vec![1, 2, 3].into(),
			}),
		];

		let genesis_config = MockGenesisConfig {
			paras: GenesisConfig { paras, ..Default::default() },
			configuration: crate::configuration::GenesisConfig {
				config: HostConfiguration {
					acceptance_period,
					validation_upgrade_delay,
					..Default::default()
				},
				..Default::default()
			},
			..Default::default()
		};

		new_test_ext(genesis_config).execute_with(|| {
			let para_id = ParaId::from(0);
			let new_code = ValidationCode(vec![4, 5, 6]);

			run_to_block(2, None);
			assert_eq!(Paras::current_code(&para_id), Some(vec![1, 2, 3].into()));

			let applied_after = {
				// this parablock is in the context of block 1.
				let applied_after = 1 + validation_upgrade_delay;
				Paras::schedule_code_upgrade(para_id, new_code.clone(), applied_after);
				Paras::note_new_head(para_id, Default::default(), 1);

				assert!(Paras::past_code_meta(&para_id).most_recent_change().is_none());
				assert_eq!(<Paras as Store>::FutureCodeUpgrades::get(&para_id), Some(applied_after));
				assert_eq!(<Paras as Store>::FutureCode::get(&para_id), new_code);
				assert_eq!(Paras::current_code(&para_id), Some(vec![1, 2, 3].into()));

				applied_after
			};

			run_to_block(applied_after, None);

			// the candidate is in the context of the parent of `applied_after`,
			// thus does not trigger the code upgrade.
			{
				Paras::note_new_head(para_id, Default::default(), applied_after - 1);

				assert!(Paras::past_code_meta(&para_id).most_recent_change().is_none());
				assert_eq!(<Paras as Store>::FutureCodeUpgrades::get(&para_id), Some(applied_after));
				assert_eq!(<Paras as Store>::FutureCode::get(&para_id), new_code);
				assert_eq!(Paras::current_code(&para_id), Some(vec![1, 2, 3].into()));
			}

			run_to_block(applied_after + 1, None);

			// the candidate is in the context of `applied_after`, and triggers
			// the upgrade.
			{
				Paras::note_new_head(para_id, Default::default(), applied_after);

				assert_eq!(
					Paras::past_code_meta(&para_id).most_recent_change(),
					Some(applied_after),
				);
				assert_eq!(
					<Paras as Store>::PastCode::get(&(para_id, applied_after)),
					Some(vec![1, 2, 3,].into()),
				);
				assert!(<Paras as Store>::FutureCodeUpgrades::get(&para_id).is_none());
				assert!(<Paras as Store>::FutureCode::get(&para_id).0.is_empty());
				assert_eq!(Paras::current_code(&para_id), Some(new_code));
			}
		});
	}

	#[test]
	fn code_upgrade_applied_after_delay_even_when_late() {
		let acceptance_period = 10;
		let validation_upgrade_delay = 5;

		let paras = vec![
			(0u32.into(), ParaGenesisArgs {
				parachain: true,
				genesis_head: Default::default(),
				validation_code: vec![1, 2, 3].into(),
			}),
		];

		let genesis_config = MockGenesisConfig {
			paras: GenesisConfig { paras, ..Default::default() },
			configuration: crate::configuration::GenesisConfig {
				config: HostConfiguration {
					acceptance_period,
					validation_upgrade_delay,
					..Default::default()
				},
				..Default::default()
			},
			..Default::default()
		};

		new_test_ext(genesis_config).execute_with(|| {
			let para_id = ParaId::from(0);
			let new_code = ValidationCode(vec![4, 5, 6]);

			run_to_block(2, None);
			assert_eq!(Paras::current_code(&para_id), Some(vec![1, 2, 3].into()));

			let applied_after = {
				// this parablock is in the context of block 1.
				let applied_after = 1 + validation_upgrade_delay;
				Paras::schedule_code_upgrade(para_id, new_code.clone(), applied_after);
				Paras::note_new_head(para_id, Default::default(), 1);

				assert!(Paras::past_code_meta(&para_id).most_recent_change().is_none());
				assert_eq!(<Paras as Store>::FutureCodeUpgrades::get(&para_id), Some(applied_after));
				assert_eq!(<Paras as Store>::FutureCode::get(&para_id), new_code);
				assert_eq!(Paras::current_code(&para_id), Some(vec![1, 2, 3].into()));

				applied_after
			};

			run_to_block(applied_after + 1 + 4, None);

			// the candidate is in the context of the first descendent of `applied_after`, and triggers
			// the upgrade.
			{
				Paras::note_new_head(para_id, Default::default(), applied_after + 4);

				assert_eq!(
					Paras::past_code_meta(&para_id).most_recent_change(),
					Some(applied_after),
				);
				assert_eq!(
					<Paras as Store>::PastCode::get(&(para_id, applied_after)),
					Some(vec![1, 2, 3,].into()),
				);
				assert!(<Paras as Store>::FutureCodeUpgrades::get(&para_id).is_none());
				assert!(<Paras as Store>::FutureCode::get(&para_id).0.is_empty());
				assert_eq!(Paras::current_code(&para_id), Some(new_code));
			}
		});
	}

	#[test]
	fn submit_code_change_when_not_allowed_is_err() {
		let acceptance_period = 10;

		let paras = vec![
			(0u32.into(), ParaGenesisArgs {
				parachain: true,
				genesis_head: Default::default(),
				validation_code: vec![1, 2, 3].into(),
			}),
		];

		let genesis_config = MockGenesisConfig {
			paras: GenesisConfig { paras, ..Default::default() },
			configuration: crate::configuration::GenesisConfig {
				config: HostConfiguration {
					acceptance_period,
					..Default::default()
				},
				..Default::default()
			},
			..Default::default()
		};

		new_test_ext(genesis_config).execute_with(|| {
			let para_id = ParaId::from(0);
			let new_code = ValidationCode(vec![4, 5, 6]);
			let newer_code = ValidationCode(vec![4, 5, 6, 7]);

			run_to_block(1, None);

			Paras::schedule_code_upgrade(para_id, new_code.clone(), 8);
			assert_eq!(<Paras as Store>::FutureCodeUpgrades::get(&para_id), Some(8));
			assert_eq!(<Paras as Store>::FutureCode::get(&para_id), new_code);

			Paras::schedule_code_upgrade(para_id, newer_code.clone(), 10);
			assert_eq!(<Paras as Store>::FutureCodeUpgrades::get(&para_id), Some(8));
			assert_eq!(<Paras as Store>::FutureCode::get(&para_id), new_code);
		});
	}

	#[test]
	fn full_parachain_cleanup_storage() {
		let acceptance_period = 10;

		let paras = vec![
			(0u32.into(), ParaGenesisArgs {
				parachain: true,
				genesis_head: Default::default(),
				validation_code: vec![1, 2, 3].into(),
			}),
		];

		let genesis_config = MockGenesisConfig {
			paras: GenesisConfig { paras, ..Default::default() },
			configuration: crate::configuration::GenesisConfig {
				config: HostConfiguration {
					acceptance_period,
					..Default::default()
				},
				..Default::default()
			},
			..Default::default()
		};

		new_test_ext(genesis_config).execute_with(|| {
			let para_id = ParaId::from(0);
			let new_code = ValidationCode(vec![4, 5, 6]);

			run_to_block(2, None);
			assert_eq!(Paras::current_code(&para_id), Some(vec![1, 2, 3].into()));

			let applied_after = {
				// this parablock is in the context of block 1.
				let applied_after = 1 + 5;
				Paras::schedule_code_upgrade(para_id, new_code.clone(), applied_after);
				Paras::note_new_head(para_id, Default::default(), 1);

				assert!(Paras::past_code_meta(&para_id).most_recent_change().is_none());
				assert_eq!(<Paras as Store>::FutureCodeUpgrades::get(&para_id), Some(applied_after));
				assert_eq!(<Paras as Store>::FutureCode::get(&para_id), new_code);
				assert_eq!(Paras::current_code(&para_id), Some(vec![1, 2, 3].into()));

				applied_after
			};

			Paras::schedule_para_cleanup(para_id);

			// Just scheduling cleanup shouldn't change anything.
			{
				assert_eq!(<Paras as Store>::OutgoingParas::get(), vec![para_id]);
				assert_eq!(Paras::parachains(), vec![para_id]);

				assert!(Paras::past_code_meta(&para_id).most_recent_change().is_none());
				assert_eq!(<Paras as Store>::FutureCodeUpgrades::get(&para_id), Some(applied_after));
				assert_eq!(<Paras as Store>::FutureCode::get(&para_id), new_code);
				assert_eq!(Paras::current_code(&para_id), Some(vec![1, 2, 3].into()));

				assert_eq!(<Paras as Store>::Heads::get(&para_id), Some(Default::default()));
			}

			// run to block, with a session change at that block.
			run_to_block(3, Some(vec![3]));

			// cleaning up the parachain should place the current parachain code
			// into the past code buffer & schedule cleanup.
			assert_eq!(Paras::past_code_meta(&para_id).most_recent_change(), Some(3));
			assert_eq!(<Paras as Store>::PastCode::get(&(para_id, 3)), Some(vec![1, 2, 3].into()));
			assert_eq!(<Paras as Store>::PastCodePruning::get(), vec![(para_id, 3)]);

			// any future upgrades haven't been used to validate yet, so those
			// are cleaned up immediately.
			assert!(<Paras as Store>::FutureCodeUpgrades::get(&para_id).is_none());
			assert!(<Paras as Store>::FutureCode::get(&para_id).0.is_empty());
			assert!(Paras::current_code(&para_id).is_none());

			// run to do the final cleanup
			let cleaned_up_at = 3 + acceptance_period + 1;
			run_to_block(cleaned_up_at, None);

			// now the final cleanup: last past code cleaned up, and this triggers meta cleanup.
			assert_eq!(Paras::past_code_meta(&para_id), Default::default());
			assert!(<Paras as Store>::PastCode::get(&(para_id, 3)).is_none());
			assert!(<Paras as Store>::PastCodePruning::get().is_empty());
		});
	}

	// TODO [now]: code_at
	// TODO [now]: registration & deregistration
}
