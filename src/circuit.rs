use std::collections::HashMap;
use std::marker::PhantomData;

use crate::FibonacciError;

use halo2_proofs::{
    circuit::{AssignedCell, Layouter, SimpleFloorPlanner},
    plonk::{Advice, Circuit, Column, ConstraintSystem, Error, Instance, Selector},
    poly::Rotation,
};
use halo2curves::{bn256::Bn256, ff::Field};

use plonkish_backend::{
    backend::{
        hyperplonk::{HyperPlonk, HyperPlonkProverParam, HyperPlonkVerifierParam},
        PlonkishBackend,
    },
    frontend::halo2::{CircuitExt, Halo2Circuit},
    halo2_curves::bn256::Fr,
    pcs::{
        multilinear,
        univariate::{self, UnivariateKzgParam},
    },
    util::{
        test::std_rng,
        transcript::{InMemoryTranscript, Keccak256Transcript},
    },
};

use rand::RngCore;

type GeminiKzg = multilinear::Gemini<univariate::UnivariateKzg<Bn256>>;
type ProvingBackend = HyperPlonk<GeminiKzg>;

/// Defines the configuration of all the columns, and all of the column definitions
/// Will be incrementally populated and passed around
#[derive(Debug, Clone)]
pub struct FibonacciConfig {
    pub col_a: Column<Advice>,
    pub col_b: Column<Advice>,
    pub col_c: Column<Advice>,
    pub selector: Selector,
    pub instance: Column<Instance>,
}

#[derive(Debug, Clone)]
struct FibonacciChip<F: Field> {
    config: FibonacciConfig,
    _marker: PhantomData<F>,
    // In rust, when you have a struct that is generic over a type parameter (here F),
    // but the type parameter is not referenced in a field of the struct,
    // you have to use PhantomData to virtually reference the type parameter,
    // so that the compiler can track it.  Otherwise it would give an error. - Jason
}

impl<F: Field> FibonacciChip<F> {
    // Default constructor
    pub fn construct(config: FibonacciConfig) -> Self {
        Self {
            config,
            _marker: PhantomData,
        }
    }

    // Configure will set what type of columns things are, enable equality, create gates, and return a config with all the gates
    pub fn configure(meta: &mut ConstraintSystem<F>) -> FibonacciConfig {
        let col_a = meta.advice_column();
        let col_b = meta.advice_column();
        let col_c = meta.advice_column();
        let selector = meta.selector();
        let instance = meta.instance_column();

        // enable_equality has some cost, so we only want to define it on rows where we need copy constraints
        meta.enable_equality(col_a);
        meta.enable_equality(col_b);
        meta.enable_equality(col_c);
        meta.enable_equality(instance);

        // Defining a create_gate here applies it over every single column in the circuit.
        // We will use the selector column to decide when to turn this gate on and off, since we probably don't want it on every row
        meta.create_gate("add", |meta| {
            //
            // col_a | col_b | col_c | selector
            //   a      b        c       s
            //
            let s = meta.query_selector(selector);
            let a = meta.query_advice(col_a, Rotation::cur());
            let b = meta.query_advice(col_b, Rotation::cur());
            let c = meta.query_advice(col_c, Rotation::cur());
            vec![s * (a + b - c)]
        });

        FibonacciConfig {
            col_a,
            col_b,
            col_c,
            selector,
            instance,
        }
    }

    // These assign functions are to be called by the synthesizer, and will be used to assign values to the columns (the witness)
    // The layouter will collect all the region definitions and compress it horizontally (i.e. squeeze up/down)
    // but not vertically (i.e. will not squeeze left/right, at least right now)
    #[allow(clippy::type_complexity)]
    pub fn assign_first_row(
        &self,
        mut layouter: impl Layouter<F>,
    ) -> Result<(AssignedCell<F, F>, AssignedCell<F, F>, AssignedCell<F, F>), Error> {
        layouter.assign_region(
            || "first row",
            |mut region| {
                self.config.selector.enable(&mut region, 0)?;

                let a_cell = region.assign_advice_from_instance(
                    || "f(0)",
                    self.config.instance,
                    0,
                    self.config.col_a,
                    0,
                )?;

                let b_cell = region.assign_advice_from_instance(
                    || "f(1)",
                    self.config.instance,
                    1,
                    self.config.col_b,
                    0,
                )?;

                let c_cell = region.assign_advice(
                    || "a + b",
                    self.config.col_c,
                    0,
                    || a_cell.value().copied() + b_cell.value(),
                )?;

                Ok((a_cell, b_cell, c_cell))
            },
        )
    }

    // This will be repeatedly called. Note that each time it makes a new region, comprised of a, b, c, s that happen to all be in the same row
    pub fn assign_row(
        &self,
        mut layouter: impl Layouter<F>,
        prev_b: &AssignedCell<F, F>,
        prev_c: &AssignedCell<F, F>,
    ) -> Result<AssignedCell<F, F>, Error> {
        layouter.assign_region(
            || "next row",
            |mut region| {
                self.config.selector.enable(&mut region, 0)?;

                // Copy the value from b & c in previous row to a & b in current row
                prev_b.copy_advice(|| "a", &mut region, self.config.col_a, 0)?;
                prev_c.copy_advice(|| "b", &mut region, self.config.col_b, 0)?;

                let c_cell = region.assign_advice(
                    || "c",
                    self.config.col_c,
                    0,
                    || prev_b.value().copied() + prev_c.value(),
                )?;

                Ok(c_cell)
            },
        )
    }

    pub fn expose_public(
        &self,
        mut layouter: impl Layouter<F>,
        cell: &AssignedCell<F, F>,
        row: usize,
    ) -> Result<(), Error> {
        layouter.constrain_instance(cell.cell(), self.config.instance, row)
    }
}

#[derive(Clone, Default)]
pub struct FibonacciCircuit<F> {
    pub public_input: Vec<Vec<F>>,
}

// Our circuit will instantiate an instance based on the interface defined on the chip and floorplanner (layouter)
// There isn't a clear reason this and the chip aren't the same thing, except for better abstractions for complex circuits
impl<F: Field> Circuit<F> for FibonacciCircuit<F> {
    type Config = FibonacciConfig;
    type FloorPlanner = SimpleFloorPlanner;

    // Circuit without witnesses, called only during key generation
    fn without_witnesses(&self) -> Self {
        Self::default()
    }

    // Has the arrangement of columns. Called only during keygen, and will just call chip config most of the time
    fn configure(meta: &mut ConstraintSystem<F>) -> Self::Config {
        FibonacciChip::configure(meta)
    }

    // Take the output of configure and floorplanner type to make the actual circuit
    // Called both at key generation time, and proving time with a specific witness
    // Will call all of the copy constraints
    fn synthesize(
        &self,
        config: Self::Config,
        mut layouter: impl Layouter<F>,
    ) -> Result<(), Error> {
        let chip = FibonacciChip::construct(config);

        let (_, mut prev_b, mut prev_c) =
            chip.assign_first_row(layouter.namespace(|| "first row"))?;

        for _i in 3..10 {
            let c_cell = chip.assign_row(layouter.namespace(|| "next row"), &prev_b, &prev_c)?;
            prev_b = prev_c;
            prev_c = c_cell;
        }

        chip.expose_public(layouter.namespace(|| "out"), &prev_c, 2)?;

        Ok(())
    }
}

impl<F: Field> CircuitExt<F> for FibonacciCircuit<F> {
    fn rand(_: usize, _: impl RngCore) -> Self {
        unimplemented!()
    }

    fn instances(&self) -> Vec<Vec<F>> {
        self.public_input.clone()
    }
}

pub(crate) fn generate_halo2_proof(
    srs: &UnivariateKzgParam<Bn256>,
    prover_parameters: &HyperPlonkProverParam<Fr, GeminiKzg>,
    inputs: HashMap<String, Vec<Fr>>,
) -> Result<(Vec<u8>, Vec<Fr>), FibonacciError> {
    // Retrieve k from SRS file
    let k = srs.k();

    // Setup starting values of the Fibonacci sequence
    let a = Fr::from(1); // F[0]
    let b = Fr::from(1); // F[1]

    // `out` value right now must be 55, but will be replaced with the actual output value
    let out: Fr = inputs
        .get("out")
        .ok_or(FibonacciError("Failed to get `out` value".to_string()))?
        .get(0)
        .ok_or(FibonacciError("Failed to get `out` value".to_string()))?
        .clone();

    let public_input = vec![a, b, out];
    let circuit = FibonacciCircuit::<Fr> {
        public_input: vec![public_input.clone()],
    };

    let halo2_circuit = Halo2Circuit::<Fr, FibonacciCircuit<Fr>>::new::<ProvingBackend>(k, circuit);

    let proof_transcript = {
        let mut proof_transcript = Keccak256Transcript::new(());

        HyperPlonk::prove(
            &prover_parameters,
            &halo2_circuit,
            &mut proof_transcript,
            std_rng(),
        )
        .unwrap();
        proof_transcript
    };

    let proof = proof_transcript.into_proof();

    Ok((proof, public_input))
}

pub(crate) fn verify_halo2_proof(
    _srs: &UnivariateKzgParam<Bn256>,
    verifier_parameters: &HyperPlonkVerifierParam<Fr, GeminiKzg>,
    proof: Vec<u8>,
    inputs: Vec<Fr>,
) -> Result<bool, FibonacciError> {
    let mut transcript;
    let result: Result<(), plonkish_backend::Error> = {
        transcript = Keccak256Transcript::from_proof((), proof.as_slice());
        ProvingBackend::verify(&verifier_parameters, &[inputs], &mut transcript, std_rng())
    };

    result
        .map(|_| true)
        .map_err(|e| FibonacciError(format!("Verifying proof error: {:?}", e)))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use crate::circuit::{generate_halo2_proof, verify_halo2_proof};

    use super::FibonacciCircuit;

    use rand::{
        rngs::{OsRng, StdRng},
        CryptoRng, RngCore, SeedableRng,
    };

    use halo2curves::bn256::Bn256;
    use plonkish_backend::{
        backend::{
            hyperplonk::{HyperPlonk, HyperPlonkProverParam, HyperPlonkVerifierParam},
            PlonkishBackend, PlonkishCircuit,
        },
        frontend::halo2::Halo2Circuit,
        halo2_curves::bn256::Fr,
        pcs::{
            multilinear,
            univariate::{self, UnivariateKzgParam},
        },
        util::transcript::{InMemoryTranscript, Keccak256Transcript},
        Error::InvalidSumcheck,
    };

    type GeminiKzg = multilinear::Gemini<univariate::UnivariateKzg<Bn256>>;
    type ProvingBackend = HyperPlonk<GeminiKzg>;

    pub fn seeded_std_rng() -> impl RngCore + CryptoRng {
        StdRng::seed_from_u64(OsRng.next_u64())
    }

    pub fn initialize_params_and_circuit(
        k: usize,
        public_input: Vec<Fr>,
    ) -> (
        Halo2Circuit<Fr, FibonacciCircuit<Fr>>,
        UnivariateKzgParam<Bn256>,
        HyperPlonkProverParam<Fr, GeminiKzg>,
        HyperPlonkVerifierParam<Fr, GeminiKzg>,
    ) {
        let circuit = FibonacciCircuit::<Fr> {
            public_input: vec![public_input.clone()],
        };

        let circuit_fn = |k| {
            let circuit =
                Halo2Circuit::<Fr, FibonacciCircuit<Fr>>::new::<ProvingBackend>(k, circuit.clone());
            (circuit.circuit_info().unwrap(), circuit)
        };
        let (circuit_info, circuit) = circuit_fn(k as usize);

        let param = ProvingBackend::setup(&circuit_info, seeded_std_rng()).unwrap();

        let (prover_parameters, verifier_parameters) =
            ProvingBackend::preprocess(&param, &circuit_info).unwrap();

        (circuit, param, prover_parameters, verifier_parameters)
    }

    // Test HyperPlonk implementation, specifically Gemini
    #[test]
    fn fibonacci_circuit_test() {
        type GeminiKzg = multilinear::Gemini<univariate::UnivariateKzg<Bn256>>;
        type ProvingBackend = HyperPlonk<GeminiKzg>;

        let a = Fr::from(1);
        let b = Fr::from(1);

        let public_input = vec![a, b, Fr::from(55)];

        let (circuit, _, prover_prarmeters, verifier_parameters) =
            initialize_params_and_circuit(4, public_input.clone());

        // Generating Proof
        let proof_transcript = {
            let mut proof_transcript = Keccak256Transcript::new(());

            HyperPlonk::prove(
                &prover_prarmeters,
                &circuit,
                &mut proof_transcript,
                seeded_std_rng(),
            )
            .unwrap();
            proof_transcript
        };

        let proof = proof_transcript.into_proof();

        // Verifying Proof
        let mut transcript;
        let result: Result<(), plonkish_backend::Error> = {
            transcript = Keccak256Transcript::from_proof((), proof.as_slice());
            ProvingBackend::verify(
                &verifier_parameters,
                &[public_input],
                &mut transcript,
                seeded_std_rng(),
            )
        };

        assert_eq!(result, Ok(()));

        let invalid_public_input = vec![a, b, Fr::from(56)];

        let invalid_result_with_wrong_input = {
            transcript = Keccak256Transcript::from_proof((), proof.as_slice());
            ProvingBackend::verify(
                &verifier_parameters,
                &[invalid_public_input],
                &mut transcript,
                seeded_std_rng(), // This is not being used in the HyperPlonk implementation
            )
        };

        assert_eq!(
            invalid_result_with_wrong_input,
            Err(InvalidSumcheck(
                "Consistency failure at round 1".to_string()
            ))
        )
    }

    #[cfg(feature = "dev-graph")]
    #[test]
    fn plot_fibonacci() {
        use plotters::prelude::*;

        let root = BitMapBackend::new("fib-1-layout.png", (1024, 3096)).into_drawing_area();
        root.fill(&WHITE).unwrap();
        let root = root.titled("Fib 1 Layout", ("sans-serif", 60)).unwrap();

        let a = Fr::from(1); // F[0]
        let b = Fr::from(1); // F[1]
        let out = Fr::from(55); // F[9]

        let public_input = vec![a, b, out];
        let circuit = FibonacciCircuit::<Fr> {
            public_input: vec![public_input],
        };

        halo2_proofs::dev::CircuitLayout::default()
            .render(4, &circuit, &root)
            .unwrap();
    }

    #[test]
    fn test_helper_functions() {
        let mut input = HashMap::new();
        input.insert("out".to_string(), vec![Fr::from(55)]);

        let public_input = vec![Fr::from(1), Fr::from(1), Fr::from(55)];
        let (_, srs, pp, vp) = initialize_params_and_circuit(4, public_input.clone());

        let (proof, inputs) = generate_halo2_proof(&srs, &pp, input).unwrap();

        assert_eq!(inputs, public_input);

        let result = verify_halo2_proof(&srs, &vp, proof, inputs);
        assert_eq!(result.unwrap(), true);
    }

    #[test]
    fn test_bad_proof_not_verified() {
        let mut input = HashMap::new();
        input.insert("out".to_string(), vec![Fr::from(56)]);

        let invalid_public_input = vec![Fr::from(1), Fr::from(1), Fr::from(56)];
        let (_, srs, pp, vp) = initialize_params_and_circuit(4, invalid_public_input.clone());

        let (proof, inputs) = generate_halo2_proof(&srs, &pp, input).unwrap();

        assert_eq!(inputs, invalid_public_input);

        let verified = verify_halo2_proof(&srs, &vp, proof, inputs).unwrap_or(false);
        assert!(!verified);
    }
}
