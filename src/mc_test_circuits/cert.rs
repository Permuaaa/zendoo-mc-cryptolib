use crate::type_mapping::*;
use algebra::ToConstraintField;
use cctp_primitives::{
    proving_system::{
        error::ProvingSystemError,
        init::{get_g1_committer_key, get_g2_committer_key},
        ProvingSystem, ZendooProof, ZendooProverKey, ZendooVerifierKey,
    },
    type_mapping::FieldElement,
    utils::{data_structures::BackwardTransfer, get_cert_data_hash},
};
use proof_systems::darlin::data_structures::{FinalDarlinDeferredData, FinalDarlinProof};
use r1cs_core::{ConstraintSynthesizer, ConstraintSystemAbstract, SynthesisError};
use r1cs_std::{
    alloc::AllocGadget, bits::boolean::Boolean, eq::EqGadget, instantiated::tweedle::FrGadget,
};
use rand::rngs::OsRng;
use cctp_primitives::proving_system::verifier::ceased_sidechain_withdrawal::PHANTOM_CERT_DATA_HASH;

type FieldElementGadget = FrGadget;

fn enforce_cert_inputs_gadget<CS: ConstraintSystemAbstract<FieldElement>>(
    mut cs: CS,
    constant_present: bool,
    constant: Option<FieldElement>,
    cert_data_hash: Option<FieldElement>,
    sc_prev_hash: Option<FieldElement>,
    num_constraints: u32,
) -> Result<(), SynthesisError> {
    let mut num_loops = 1; // cert_data_hash
    if constant_present {
        num_loops += 1;    // [constant]
    }
    if sc_prev_hash.is_some() {
        num_loops += 1;    // [sc_prev_hash]
    }

    let constraints_per_loop = 4; // is_eq adds 3 constraints, enforce_equal adds 1 constraint

    let num_iterations = ((num_constraints - 1) / num_loops) / constraints_per_loop;

    if constant_present {
        let constant_g = FieldElementGadget::alloc(cs.ns(|| "alloc constant"), || {
            constant.ok_or(SynthesisError::AssignmentMissing)
        })?;

        let expected_constant_g =
            FieldElementGadget::alloc_input(cs.ns(|| "alloc expected_constant"), || {
                constant.ok_or(SynthesisError::AssignmentMissing)
            })?;

        for i in 0..num_iterations {
            let b = constant_g.is_eq(
                cs.ns(|| format!("expected_constant_is_eq_actual_{}", i)),
                &expected_constant_g,
            )?;
            b.enforce_equal(
                cs.ns(|| format!("expected_constant_must_be_eq_actual_{}", i)),
                &Boolean::Constant(true),
            )?;
        }
    }

    let cert_data_hash_g = FieldElementGadget::alloc(cs.ns(|| "alloc cert_data_hash"), || {
        cert_data_hash.ok_or(SynthesisError::AssignmentMissing)
    })?;

    let expected_cert_data_hash_g =
        FieldElementGadget::alloc_input(cs.ns(|| "alloc expected_cert_data_hash"), || {
            cert_data_hash.ok_or(SynthesisError::AssignmentMissing)
        })?;

    let mut sc_prev_hash_g: Option<FieldElementGadget> = None;
    let mut expected_sc_prev_hash_g: Option<FieldElementGadget> = None;
    if sc_prev_hash.is_some() {
        sc_prev_hash_g = Some(FieldElementGadget::alloc(cs.ns(|| "alloc sc_prev_cert_hash"), || {
            sc_prev_hash.ok_or(SynthesisError::AssignmentMissing)
        })?);

        expected_sc_prev_hash_g =
            Some(FieldElementGadget::alloc_input(cs.ns(|| "alloc expected_sc_prev_cert_hash"), || {
                sc_prev_hash.ok_or(SynthesisError::AssignmentMissing)
            })?);
    }

    for i in 0..num_iterations {
        let b = cert_data_hash_g.is_eq(
            cs.ns(|| format!("expected_cert_data_hash_is_eq_actual_{}", i)),
            &expected_cert_data_hash_g,
        )?;
        b.enforce_equal(
            cs.ns(|| format!("expected_cert_data_hash_must_be_eq_actual_{}", i)),
            &Boolean::Constant(true),
        )?;
    }

    if sc_prev_hash_g.is_some() {
        let prev_hash = sc_prev_hash_g.unwrap();
        let expected_prev_hash = expected_sc_prev_hash_g.unwrap();

        for i in 0..num_iterations {
            let c = prev_hash.is_eq(
                cs.ns(|| format!("expected_sc_prev_hash_is_eq_actual_{}", i)),
                &expected_prev_hash,
            )?;
            c.enforce_equal(
                cs.ns(|| format!("expected_sc_prev_hash_must_be_eq_actual_{}", i)),
                &Boolean::Constant(true),
            )?;
        }
    }

    Ok(())
}

// Simple test circuit, enforcing that the hash of all the witnesses equals the public input
#[derive(Debug)]
pub struct CertTestCircuit {
    constant_present: bool,
    constant: Option<FieldElement>,
    cert_data_hash: Option<FieldElement>,
    sc_prev_cert_hash: Option<FieldElement>,
    num_constraints: u32,
}

impl ConstraintSynthesizer<FieldElement> for CertTestCircuit {
    fn generate_constraints<CS: ConstraintSystemAbstract<FieldElement>>(
        self,
        cs: &mut CS,
    ) -> Result<(), SynthesisError> {
        assert!(self.num_constraints > 2);
        enforce_cert_inputs_gadget(
            cs.ns(|| "enforce witnesses == pub_ins"),
            self.constant_present,
            self.constant,
            self.cert_data_hash,
            self.sc_prev_cert_hash,
            self.num_constraints,
        )
    }
}

// Simple test circuit, enforcing that the hash of all the witnesses equals the public input
pub struct CertTestCircuitWithAccumulators {
    constant_present: bool,
    constant: Option<FieldElement>,
    cert_data_hash: Option<FieldElement>,
    sc_prev_cert_hash: Option<FieldElement>,
    deferred: Vec<FieldElement>, // Represents deferred data
    num_constraints: u32,
}

impl ConstraintSynthesizer<FieldElement> for CertTestCircuitWithAccumulators {
    fn generate_constraints<CS: ConstraintSystemAbstract<FieldElement>>(
        self,
        cs: &mut CS,
    ) -> Result<(), SynthesisError> {
        assert!(self.num_constraints > 2);

        let deferred_len = self.deferred.len() as u32;
        assert!(self.num_constraints - 1 > deferred_len);

        // convert the FinalDarlinDeferred efficiently to circuit inputs
        let deferred_as_native_fes = self.deferred;

        // Alloc deferred data as public input
        let mut deferred_input_gs = Vec::new();
        for (i, fe) in deferred_as_native_fes.iter().enumerate() {
            let ins_g = FieldElementGadget::alloc_input(
                cs.ns(|| format!("Alloc input deferred elem {}", i)),
                || Ok(fe),
            )?;
            deferred_input_gs.push(ins_g);
        }

        // Alloc deferred data as witness
        let mut deferred_gs = Vec::new();
        for (i, fe) in deferred_as_native_fes.into_iter().enumerate() {
            let witness_g =
                FieldElementGadget::alloc(cs.ns(|| format!("Alloc deferred elem {}", i)), || {
                    Ok(fe)
                })?;
            deferred_gs.push(witness_g);
        }

        // Enforce the system inputs to the circuit to be equal to the allocated `deferred`.
        // This is a simple way to allow test cases where sys data (i.e. the deferred
        // accumulators) are wrong.
        for (i, (deferred_w, deferred_ins)) in
            deferred_input_gs.into_iter().zip(deferred_gs).enumerate()
        {
            deferred_w.enforce_equal(
                cs.ns(|| format!("enforce deferred equal {}", i)),
                &deferred_ins,
            )?;
        }

        enforce_cert_inputs_gadget(
            cs.ns(|| "enforce witnesses == pub_ins"),
            self.constant_present,
            self.constant,
            self.cert_data_hash,
            self.sc_prev_cert_hash,
            self.num_constraints - deferred_len,
        )?;

        Ok(())
    }
}

pub fn generate_parameters(
    ps: ProvingSystem,
    num_constraints: u32,
    with_constant: bool,
    segment_size: Option<u32>,
    with_prev_hash: bool,
) -> Result<(ZendooProverKey, ZendooVerifierKey), ProvingSystemError> {
    let supported_degree = segment_size.map(|ss| (ss - 1) as usize);
    let ck_g1 = get_g1_committer_key(supported_degree)?;
    match ps {
        ProvingSystem::Undefined => Err(ProvingSystemError::UndefinedProvingSystem),
        ProvingSystem::Darlin => {
            let ck_g2 = get_g2_committer_key(supported_degree)?;
            let circ = CertTestCircuitWithAccumulators {
                constant_present: with_constant,
                constant: None,
                cert_data_hash: None,
                sc_prev_cert_hash: if with_prev_hash {
                    Some(PHANTOM_CERT_DATA_HASH)
                } else {
                    None
                },
                deferred: FinalDarlinDeferredData::<G1, G2>::generate_random::<_, Digest>(
                    &mut rand::thread_rng(),
                    &ck_g1,
                    &ck_g2,
                )
                .to_field_elements()
                .unwrap(),
                num_constraints,
            };
            let (pk, vk) = CoboundaryMarlin::index(&ck_g1, circ)
                .map_err(|e| ProvingSystemError::SetupFailed(e.to_string()))?;
            Ok((ZendooProverKey::Darlin(pk), ZendooVerifierKey::Darlin(vk)))
        }
        ProvingSystem::CoboundaryMarlin => {
            let circ = CertTestCircuit {
                constant_present: with_constant,
                constant: None,
                cert_data_hash: None,
                sc_prev_cert_hash: if with_prev_hash {
                    Some(PHANTOM_CERT_DATA_HASH)
                } else {
                    None
                },
                num_constraints,
            };
            let (pk, vk) = CoboundaryMarlin::index(&ck_g1, circ)
                .map_err(|e| ProvingSystemError::SetupFailed(e.to_string()))?;
            Ok((
                ZendooProverKey::CoboundaryMarlin(pk),
                ZendooVerifierKey::CoboundaryMarlin(vk),
            ))
        }
    }
}

pub fn generate_proof(
    pk: &ZendooProverKey,
    zk: bool,
    constant: Option<&FieldElement>,
    sc_id: &FieldElement,
    epoch_number: u32,
    quality: u64,
    custom_fields: Option<Vec<&FieldElement>>,
    bt_list: Option<&[BackwardTransfer]>,
    end_cumulative_sc_tx_commitment_tree_root: &FieldElement,
    btr_fee: u64,
    ft_min_amount: u64,
    num_constraints: u32,
    segment_size: Option<u32>,
    sc_prev_hash: Option<&FieldElement>,
) -> Result<ZendooProof, ProvingSystemError> {
    let supported_degree = segment_size.map(|ss| (ss - 1) as usize);
    let rng = &mut OsRng;
    let ck_g1 = get_g1_committer_key(supported_degree)?;

    // Read input param into field elements
    let cert_data_hash = get_cert_data_hash(
        sc_id,
        epoch_number,
        quality,
        bt_list,
        custom_fields,
        end_cumulative_sc_tx_commitment_tree_root,
        btr_fee,
        ft_min_amount,
    )
    .map_err(|e| ProvingSystemError::ProofCreationFailed(e.to_string()))?;

    match pk {
        ZendooProverKey::Darlin(pk) => {
            let ck_g2 = get_g2_committer_key(supported_degree)?;
            let deferred = FinalDarlinDeferredData::<G1, G2>::generate_random::<_, Digest>(
                rng,
                &ck_g1,
                &ck_g2,
            );
            let deferred_fes = deferred.to_field_elements().unwrap();
            let circ = CertTestCircuitWithAccumulators {
                constant_present: constant.is_some(),
                constant: if constant.is_some() {
                    Some(*constant.unwrap())
                } else {
                    None
                },
                cert_data_hash: Some(cert_data_hash),
                sc_prev_cert_hash: if sc_prev_hash.is_some() {
                    Some(*sc_prev_hash.unwrap())
                } else {
                    None
                },
                deferred: deferred_fes.clone(),
                num_constraints,
            };
            let proof = CoboundaryMarlin::prove(
                pk,
                &ck_g1,
                circ,
                zk,
                if zk { Some(rng) } else { None },
            )
            .map_err(|e| ProvingSystemError::ProofCreationFailed(e.to_string()))?;
            let darlin_proof = FinalDarlinProof::<G1, G2, Digest> {
                proof: MarlinProof(proof),
                deferred,
            };
            Ok(ZendooProof::Darlin(darlin_proof))
        }
        ZendooProverKey::CoboundaryMarlin(pk) => {
            let circ = CertTestCircuit {
                constant_present: constant.is_some(),
                constant: if constant.is_some() {
                    Some(*constant.unwrap())
                } else {
                    None
                },
                cert_data_hash: Some(cert_data_hash),
                sc_prev_cert_hash: if sc_prev_hash.is_some() {
                    Some(*sc_prev_hash.unwrap())
                } else {
                    None
                },
                num_constraints,
            };
            let proof = CoboundaryMarlin::prove(
                pk,
                &ck_g1,
                circ,
                zk,
                if zk { Some(rng) } else { None },
            )
            .map_err(|e| ProvingSystemError::ProofCreationFailed(e.to_string()))?;
            Ok(ZendooProof::CoboundaryMarlin(MarlinProof(proof)))
        }
    }
}
