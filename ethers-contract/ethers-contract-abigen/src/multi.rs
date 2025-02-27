//! TODO

use eyre::Result;
use inflector::Inflector;
use proc_macro2::TokenStream;
use quote::quote;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs,
    io::Write,
    path::Path,
};

use crate::{util, Abigen, Context, ContractBindings, ExpandedContract};

/// Represents a collection of [`Abigen::expand()`]
pub struct MultiExpansion {
    // all expanded contracts collection from [`Abigen::expand()`]
    contracts: Vec<(ExpandedContract, Context)>,
}

impl MultiExpansion {
    /// Create a new instance that wraps the given `contracts`
    pub fn new(contracts: Vec<(ExpandedContract, Context)>) -> Self {
        Self { contracts }
    }

    /// Create a new instance by expanding all `Abigen` elements the given iterator yields
    pub fn from_abigen(abigens: impl IntoIterator<Item = Abigen>) -> Result<Self> {
        let contracts = abigens.into_iter().map(|abigen| abigen.expand()).collect::<Result<_>>()?;
        Ok(Self::new(contracts))
    }

    /// Expands all contracts into a single `TokenStream`
    ///
    /// This will deduplicate types into a separate `mod __shared_types` module, if any.
    pub fn expand_inplace(self) -> TokenStream {
        self.expand().expand_inplace()
    }

    /// Expands all contracts into separated [`TokenStream`]s
    ///
    /// If there was type deduplication, this returns a list of [`TokenStream`] containing the type
    /// definitions of all shared types.
    pub fn expand(self) -> MultiExpansionResult {
        let mut expansions = self.contracts;
        let mut shared_types = Vec::new();
        // this keeps track of those contracts that need to be updated after a struct was
        // extracted from the contract's module and moved to the shared module
        let mut dirty_contracts = HashSet::new();

        // merge all types if more than 1 contract
        if expansions.len() > 1 {
            // check for type conflicts across all contracts
            let mut conflicts: HashMap<String, Vec<usize>> = HashMap::new();
            for (idx, (_, ctx)) in expansions.iter().enumerate() {
                for type_identifier in ctx.internal_structs().rust_type_names().keys() {
                    conflicts
                        .entry(type_identifier.clone())
                        .or_insert_with(|| Vec::with_capacity(1))
                        .push(idx);
                }
            }

            // resolve type conflicts
            for (id, contracts) in conflicts.iter().filter(|(_, c)| c.len() > 1) {
                // extract the shared type once
                shared_types.push(
                    expansions[contracts[0]]
                        .1
                        .struct_definition(id)
                        .expect("struct def succeeded previously"),
                );

                // remove the shared type from the contract's bindings
                for contract in contracts.iter().copied() {
                    expansions[contract].1.remove_struct(id);
                    dirty_contracts.insert(contract);
                }
            }

            // regenerate all struct definitions that were hit
            for contract in dirty_contracts.iter().copied() {
                let (expanded, ctx) = &mut expansions[contract];
                expanded.abi_structs = ctx.abi_structs().expect("struct def succeeded previously");
            }
        }

        MultiExpansionResult { contracts: expansions, dirty_contracts, shared_types }
    }
}

/// Represents an intermediary result of [`MultiExpansion::expand()`]
pub struct MultiExpansionResult {
    contracts: Vec<(ExpandedContract, Context)>,
    /// contains the indices of contracts with structs that need to be updated
    dirty_contracts: HashSet<usize>,
    /// all type definitions of types that are shared by multiple contracts
    shared_types: Vec<TokenStream>,
}

impl MultiExpansionResult {
    /// Expands all contracts into a single [`TokenStream`]
    pub fn expand_inplace(mut self) -> TokenStream {
        let mut tokens = TokenStream::new();

        let shared_types_module = quote! {__shared_types};
        // the import path to the shared types
        let shared_path = quote!(
            pub use super::#shared_types_module::*;
        );
        self.add_shared_import_path(shared_path);

        let Self { contracts, shared_types, .. } = self;

        if !shared_types.is_empty() {
            tokens.extend(quote! {
                pub mod #shared_types_module {
                    #( #shared_types )*
                }
            });
        }

        tokens.extend(contracts.into_iter().map(|(exp, _)| exp.into_tokens()));

        tokens
    }

    /// Sets the path to the shared types module according to the value of `single_file`
    ///
    /// If `single_file` then it's expected that types will be written to `shared_types.rs`
    fn set_shared_import_path(&mut self, single_file: bool) {
        let shared_path = if single_file {
            quote!(
                pub use super::__shared_types::*;
            )
        } else {
            quote!(
                pub use super::super::shared_types::*;
            )
        };
        self.add_shared_import_path(shared_path);
    }

    /// adds the `shared` import path to every `dirty` contract
    fn add_shared_import_path(&mut self, shared: TokenStream) {
        for contract in self.dirty_contracts.iter().copied() {
            let (expanded, ..) = &mut self.contracts[contract];
            expanded.imports.extend(shared.clone());
        }
    }

    /// Converts this result into [`MultiBindingsInner`]
    fn into_bindings(mut self, single_file: bool, rustfmt: bool) -> MultiBindingsInner {
        self.set_shared_import_path(single_file);
        let Self { contracts, shared_types, .. } = self;
        let bindings = contracts
            .into_iter()
            .map(|(expanded, ctx)| ContractBindings {
                tokens: expanded.into_tokens(),
                rustfmt,
                name: ctx.contract_name().to_string(),
            })
            .map(|v| (v.name.clone(), v))
            .collect();

        let shared_types = if !shared_types.is_empty() {
            let shared_types = if single_file {
                quote! {
                    pub mod __shared_types {
                        #( #shared_types )*
                    }
                }
            } else {
                quote! {
                    #( #shared_types )*
                }
            };
            Some(ContractBindings {
                tokens: shared_types,
                rustfmt,
                name: "shared_types".to_string(),
            })
        } else {
            None
        };

        MultiBindingsInner { bindings, shared_types }
    }
}

/// Collects Abigen structs for a series of contracts, pending generation of
/// the contract bindings.
#[derive(Debug, Clone)]
pub struct MultiAbigen {
    /// Abigen objects to be written
    abigens: Vec<Abigen>,
}

impl std::ops::Deref for MultiAbigen {
    type Target = Vec<Abigen>;

    fn deref(&self) -> &Self::Target {
        &self.abigens
    }
}

impl From<Vec<Abigen>> for MultiAbigen {
    fn from(abigens: Vec<Abigen>) -> Self {
        Self { abigens }
    }
}

impl std::iter::FromIterator<Abigen> for MultiAbigen {
    fn from_iter<I: IntoIterator<Item = Abigen>>(iter: I) -> Self {
        iter.into_iter().collect::<Vec<_>>().into()
    }
}

impl MultiAbigen {
    /// Create a new instance from a series (`contract name`, `abi_source`)
    ///
    /// See `Abigen::new`
    pub fn new<I, Name, Source>(abis: I) -> Result<Self>
    where
        I: IntoIterator<Item = (Name, Source)>,
        Name: AsRef<str>,
        Source: AsRef<str>,
    {
        let abis = abis
            .into_iter()
            .map(|(contract_name, abi_source)| Abigen::new(contract_name.as_ref(), abi_source))
            .collect::<Result<Vec<_>>>()?;

        Ok(Self::from_abigens(abis))
    }

    /// Create a new instance from a series of already resolved `Abigen`
    pub fn from_abigens(abis: impl IntoIterator<Item = Abigen>) -> Self {
        abis.into_iter().collect()
    }

    /// Reads all json files contained in the given `dir` and use the file name for the name of the
    /// `ContractBindings`.
    /// This is equivalent to calling `MultiAbigen::new` with all the json files and their filename.
    ///
    /// # Example
    ///
    /// ```text
    /// abi
    /// ├── ERC20.json
    /// ├── Contract1.json
    /// ├── Contract2.json
    /// ...
    /// ```
    ///
    /// ```no_run
    /// # use ethers_contract_abigen::MultiAbigen;
    /// let gen = MultiAbigen::from_json_files("./abi").unwrap();
    /// ```
    pub fn from_json_files(root: impl AsRef<Path>) -> Result<Self> {
        util::json_files(root.as_ref()).into_iter().map(Abigen::from_file).collect()
    }

    /// Add another Abigen to the module or lib
    pub fn push(&mut self, abigen: Abigen) {
        self.abigens.push(abigen)
    }

    /// Build the contract bindings and prepare for writing
    pub fn build(self) -> Result<MultiBindings> {
        let rustfmt = self.abigens.iter().any(|gen| gen.rustfmt);
        Ok(MultiBindings {
            expansion: MultiExpansion::from_abigen(self.abigens)?.expand(),
            rustfmt,
        })
    }
}

/// Output of the [`MultiAbigen`] build process. `MultiBindings` wraps a group
/// of built contract bindings that have yet to be written to disk.
///
/// `MultiBindings` enables the user to
/// 1. Write a collection of bindings to a rust module
/// 2. Write a collection of bindings to a rust lib
/// 3. Ensure that a collection of bindings matches an on-disk module or lib.
///
/// Generally we recommend writing the bindings to a module folder within your
/// rust project. Users seeking to create "official" bindings for some project
/// may instead write an entire library to publish via crates.io.
///
/// Rather than using `MultiAbigen` in a build script, we recommend committing
/// the generated files, and replacing the build script with an integration
/// test. To enable this, we have provided
/// `MultiBindings::ensure_consistent_bindings` and
/// `MultiBindings::ensure_consistent_crate`. These functions generate the
/// expected module or library in memory, and check that the on-disk files
/// match the expected files. We recommend running these inside CI.
///
/// This has several advantages:
///   * No need for downstream users to compile the build script
///   * No need for downstream users to run the whole `abigen!` generation steps
///   * The generated code is more usable in an IDE
///   * CI will fail if the generated code is out of date (if `abigen!` or the contract's ABI itself
///     changed)
pub struct MultiBindings {
    expansion: MultiExpansionResult,
    rustfmt: bool,
}

impl MultiBindings {
    /// Returns the number of contracts to generate bindings for.
    pub fn len(&self) -> usize {
        self.expansion.contracts.len()
    }

    /// Returns whether there are any bindings to be generated
    pub fn is_empty(&self) -> bool {
        self.expansion.contracts.is_empty()
    }

    fn into_inner(self, single_file: bool) -> MultiBindingsInner {
        self.expansion.into_bindings(single_file, self.rustfmt)
    }

    /// Generates all the bindings and writes them to the given module
    ///
    /// # Example
    ///
    /// Read all json abi files from the `./abi` directory
    /// ```text
    /// abi
    /// ├── ERC20.json
    /// ├── Contract1.json
    /// ├── Contract2.json
    /// ...
    /// ```
    ///
    /// and write them to the `./src/contracts` location as
    ///
    /// ```text
    /// src/contracts
    /// ├── mod.rs
    /// ├── er20.rs
    /// ├── contract1.rs
    /// ├── contract2.rs
    /// ...
    /// ```
    ///
    /// ```no_run
    /// # use ethers_contract_abigen::MultiAbigen;
    /// let gen = MultiAbigen::from_json_files("./abi").unwrap();
    /// let bindings = gen.build().unwrap();
    /// bindings.write_to_module("./src/contracts", false).unwrap();
    /// ```
    pub fn write_to_module(self, module: impl AsRef<Path>, single_file: bool) -> Result<()> {
        self.into_inner(single_file).write_to_module(module, single_file)
    }

    /// Generates all the bindings and writes a library crate containing them
    /// to the provided path
    ///
    /// # Example
    ///
    /// Read all json abi files from the `./abi` directory
    /// ```text
    /// abi
    /// ├── ERC20.json
    /// ├── Contract1.json
    /// ├── Contract2.json
    /// ├── Contract3/
    ///     ├── Contract3.json
    /// ...
    /// ```
    ///
    /// and write them to the `./bindings` location as
    ///
    /// ```text
    /// bindings
    /// ├── Cargo.toml
    /// ├── src/
    ///     ├── lib.rs
    ///     ├── er20.rs
    ///     ├── contract1.rs
    ///     ├── contract2.rs
    /// ...
    /// ```
    ///
    /// ```no_run
    /// # use ethers_contract_abigen::MultiAbigen;
    /// let gen = MultiAbigen::from_json_files("./abi").unwrap();
    /// let bindings = gen.build().unwrap();
    /// bindings.write_to_crate(
    ///     "my-crate", "0.0.5", "./bindings", false
    /// ).unwrap();
    /// ```
    pub fn write_to_crate(
        self,
        name: impl AsRef<str>,
        version: impl AsRef<str>,
        lib: impl AsRef<Path>,
        single_file: bool,
    ) -> Result<()> {
        self.into_inner(single_file).write_to_crate(name, version, lib, single_file)
    }

    /// This ensures that the already generated bindings crate matches the
    /// output of a fresh new run. Run this in a rust test, to get notified in
    /// CI if the newly generated bindings deviate from the already generated
    /// ones, and it's time to generate them again. This could happen if the
    /// ABI of a contract or the output that `ethers` generates changed.
    ///
    /// If this functions is run within a test during CI and fails, then it's
    /// time to update all bindings.
    ///
    /// # Returns
    ///
    /// `Ok(())` if the freshly generated bindings match with the
    /// existing bindings. Otherwise an `Err(_)` containing an `eyre::Report`
    /// with more information.
    ///
    /// # Example
    ///
    /// Check that the generated files are up to date
    ///
    /// ```no_run
    /// # use ethers_contract_abigen::MultiAbigen;
    /// #[test]
    /// fn generated_bindings_are_fresh() {
    ///  let project_root = std::path::Path::new(&env!("CARGO_MANIFEST_DIR"));
    ///  let abi_dir = project_root.join("abi");
    ///  let gen = MultiAbigen::from_json_files(&abi_dir).unwrap();
    ///  let bindings = gen.build().unwrap();
    ///  bindings.ensure_consistent_crate(
    ///     "my-crate", "0.0.1", project_root.join("src/contracts"), false, true
    ///  ).expect("inconsistent bindings");
    /// }
    /// ```
    pub fn ensure_consistent_crate(
        self,
        name: impl AsRef<str>,
        version: impl AsRef<str>,
        crate_path: impl AsRef<Path>,
        single_file: bool,
        check_cargo_toml: bool,
    ) -> Result<()> {
        self.into_inner(single_file).ensure_consistent_crate(
            name,
            version,
            crate_path,
            single_file,
            check_cargo_toml,
        )
    }

    /// This ensures that the already generated bindings module matches the
    /// output of a fresh new run. Run this in a rust test, to get notified in
    /// CI if the newly generated bindings deviate from the already generated
    /// ones, and it's time to generate them again. This could happen if the
    /// ABI of a contract or the output that `ethers` generates changed.
    ///
    /// If this functions is run within a test during CI and fails, then it's
    /// time to update all bindings.
    ///
    /// # Returns
    ///
    /// `Ok(())` if the freshly generated bindings match with the
    /// existing bindings. Otherwise an `Err(_)` containing an `eyre::Report`
    /// with more information.
    ///
    /// # Example
    ///
    /// Check that the generated files are up to date
    ///
    /// ```no_run
    /// # use ethers_contract_abigen::MultiAbigen;
    /// #[test]
    /// fn generated_bindings_are_fresh() {
    ///  let project_root = std::path::Path::new(&env!("CARGO_MANIFEST_DIR"));
    ///  let abi_dir = project_root.join("abi");
    ///  let gen = MultiAbigen::from_json_files(&abi_dir).unwrap();
    ///  let bindings = gen.build().unwrap();
    ///  bindings.ensure_consistent_module(
    ///     project_root.join("src/contracts"), false
    ///  ).expect("inconsistent bindings");
    /// }
    /// ```
    pub fn ensure_consistent_module(
        self,
        module: impl AsRef<Path>,
        single_file: bool,
    ) -> Result<()> {
        self.into_inner(single_file).ensure_consistent_module(module, single_file)
    }
}

struct MultiBindingsInner {
    /// Abigen objects to be written
    bindings: BTreeMap<String, ContractBindings>,
    /// contains the content of the shared types if any
    shared_types: Option<ContractBindings>,
}

// deref allows for inspection without modification
impl std::ops::Deref for MultiBindingsInner {
    type Target = BTreeMap<String, ContractBindings>;

    fn deref(&self) -> &Self::Target {
        &self.bindings
    }
}

impl MultiBindingsInner {
    /// Generate the contents of the `Cargo.toml` file for a lib
    fn generate_cargo_toml(
        &self,
        name: impl AsRef<str>,
        version: impl AsRef<str>,
    ) -> Result<Vec<u8>> {
        let mut toml = vec![];

        writeln!(toml, "[package]")?;
        writeln!(toml, r#"name = "{}""#, name.as_ref())?;
        writeln!(toml, r#"version = "{}""#, version.as_ref())?;
        writeln!(toml, r#"edition = "2021""#)?;
        writeln!(toml)?;
        writeln!(toml, "# See more keys and their definitions at https://doc.rust-lang.org/cargo/reference/manifest.html")?;
        writeln!(toml)?;
        writeln!(toml, "[dependencies]")?;
        writeln!(
            toml,
            r#"
ethers = {{ git = "https://github.com/gakonst/ethers-rs", default-features = false }}
serde_json = "1.0.79"
"#
        )?;
        Ok(toml)
    }

    /// Write the contents of `Cargo.toml` to disk
    fn write_cargo_toml(
        &self,
        lib: &Path,
        name: impl AsRef<str>,
        version: impl AsRef<str>,
    ) -> Result<()> {
        let contents = self.generate_cargo_toml(name, version)?;

        let mut file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(lib.join("Cargo.toml"))?;
        file.write_all(&contents)?;

        Ok(())
    }

    /// Append module declarations to the `lib.rs` or `mod.rs`
    fn append_module_names(&self, mut buf: impl Write) -> Result<()> {
        let mut mod_names: BTreeSet<_> =
            self.bindings.keys().map(|name| util::safe_module_name(name)).collect();
        if let Some(ref shared) = self.shared_types {
            mod_names.insert(shared.name.to_snake_case());
        }

        for module in mod_names.into_iter().map(|name| format!("pub mod {};", name)) {
            writeln!(buf, "{}", module)?;
        }

        Ok(())
    }

    /// Generate the contents of `lib.rs` or `mod.rs`
    fn generate_super_contents(&self, is_crate: bool, single_file: bool) -> Result<Vec<u8>> {
        let mut contents = vec![];
        generate_prefix(&mut contents, is_crate, single_file)?;

        if single_file {
            if let Some(ref shared) = self.shared_types {
                shared.write(&mut contents)?;
            }
            for binding in self.bindings.values() {
                binding.write(&mut contents)?;
            }
        } else {
            self.append_module_names(&mut contents)?;
        }

        Ok(contents)
    }

    /// Write the `lib.rs` or `mod.rs` to disk
    fn write_super_file(&self, path: &Path, is_crate: bool, single_file: bool) -> Result<()> {
        let filename = if is_crate { "lib.rs" } else { "mod.rs" };
        let contents = self.generate_super_contents(is_crate, single_file)?;
        fs::write(path.join(filename), contents)?;
        Ok(())
    }

    /// Write all contract bindings to their respective files
    fn write_bindings(&self, path: &Path) -> Result<()> {
        if let Some(ref shared) = self.shared_types {
            shared.write_module_in_dir(path)?;
        }
        for binding in self.bindings.values() {
            binding.write_module_in_dir(path)?;
        }
        Ok(())
    }

    fn write_to_module(self, module: impl AsRef<Path>, single_file: bool) -> Result<()> {
        let module = module.as_ref();
        fs::create_dir_all(module)?;

        self.write_super_file(module, false, single_file)?;

        if !single_file {
            self.write_bindings(module)?;
        }
        Ok(())
    }

    fn write_to_crate(
        self,
        name: impl AsRef<str>,
        version: impl AsRef<str>,
        lib: impl AsRef<Path>,
        single_file: bool,
    ) -> Result<()> {
        let lib = lib.as_ref();
        let src = lib.join("src");
        fs::create_dir_all(&src)?;

        self.write_cargo_toml(lib, name, version)?;
        self.write_super_file(&src, true, single_file)?;

        if !single_file {
            self.write_bindings(&src)?;
        }

        Ok(())
    }

    /// Ensures the contents of the bindings directory are correct
    ///
    /// Does this by first generating the `lib.rs` or `mod.rs`, then the
    /// contents of each binding file in turn.
    fn ensure_consistent_bindings(
        self,
        dir: impl AsRef<Path>,
        is_crate: bool,
        single_file: bool,
    ) -> Result<()> {
        let dir = dir.as_ref();
        let super_name = if is_crate { "lib.rs" } else { "mod.rs" };

        let super_contents = self.generate_super_contents(is_crate, single_file)?;
        check_file_in_dir(dir, super_name, &super_contents)?;

        // If it is single file, we skip checking anything but the super
        // contents
        if !single_file {
            for binding in self.bindings.values() {
                check_binding_in_dir(dir, binding)?;
            }
        }

        Ok(())
    }

    fn ensure_consistent_crate(
        self,
        name: impl AsRef<str>,
        version: impl AsRef<str>,
        crate_path: impl AsRef<Path>,
        single_file: bool,
        check_cargo_toml: bool,
    ) -> Result<()> {
        let crate_path = crate_path.as_ref();

        if check_cargo_toml {
            // additionally check the contents of the cargo
            let cargo_contents = self.generate_cargo_toml(name, version)?;
            check_file_in_dir(crate_path, "Cargo.toml", &cargo_contents)?;
        }

        self.ensure_consistent_bindings(crate_path.join("src"), true, single_file)?;
        Ok(())
    }

    fn ensure_consistent_module(self, module: impl AsRef<Path>, single_file: bool) -> Result<()> {
        self.ensure_consistent_bindings(module, false, single_file)?;
        Ok(())
    }
}

/// Generate the shared prefix of the `lib.rs` or `mod.rs`
fn generate_prefix(mut buf: impl Write, is_crate: bool, single_file: bool) -> Result<()> {
    writeln!(buf, "#![allow(clippy::all)]")?;
    writeln!(
        buf,
        "//! This {} contains abigen! generated bindings for solidity contracts.",
        if is_crate { "lib" } else { "module" }
    )?;
    writeln!(buf, "//! This is autogenerated code.")?;
    writeln!(buf, "//! Do not manually edit these files.")?;
    writeln!(
        buf,
        "//! {} may be overwritten by the codegen system at any time.",
        if single_file && !is_crate { "This file" } else { "These files" }
    )?;
    Ok(())
}

fn check_file_in_dir(dir: &Path, file_name: &str, expected_contents: &[u8]) -> Result<()> {
    eyre::ensure!(dir.is_dir(), "Not a directory: {}", dir.display());

    let file_path = dir.join(file_name);
    eyre::ensure!(file_path.is_file(), "Not a file: {}", file_path.display());

    let contents = fs::read(&file_path).expect("Unable to read file");
    eyre::ensure!(contents == expected_contents, format!("The contents of `{}` do not match the expected output of the newest `ethers::Abigen` version.\
This indicates that the existing bindings are outdated and need to be generated again.", file_path.display()));
    Ok(())
}

fn check_binding_in_dir(dir: &Path, binding: &ContractBindings) -> Result<()> {
    let name = binding.module_filename();
    let contents = binding.to_vec();

    check_file_in_dir(dir, &name, &contents)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use ethers_solc::project_util::TempProject;
    use std::{panic, path::PathBuf};

    struct Context {
        multi_gen: MultiAbigen,
        mod_root: PathBuf,
    }

    fn run_test<T>(test: T)
    where
        T: FnOnce(&Context) + panic::UnwindSafe,
    {
        let crate_root = std::path::Path::new(&env!("CARGO_MANIFEST_DIR")).to_owned();
        let console = Abigen::new(
            "Console",
            crate_root.join("../tests/solidity-contracts/console.json").display().to_string(),
        )
        .unwrap();

        let simple_storage = Abigen::new(
            "SimpleStorage",
            crate_root
                .join("../tests/solidity-contracts/simplestorage_abi.json")
                .display()
                .to_string(),
        )
        .unwrap();

        let human_readable = Abigen::new(
            "HrContract",
            r#"[
        struct Foo { uint256 x; }
        function foo(Foo memory x)
        function bar(uint256 x, uint256 y, address addr)
        yeet(uint256,uint256,address)
    ]"#,
        )
        .unwrap();

        let multi_gen = MultiAbigen::from_abigens([console, simple_storage, human_readable]);

        let mod_root = tempfile::tempdir().unwrap().path().join("contracts");
        let context = Context { multi_gen, mod_root };

        let result = panic::catch_unwind(|| test(&context));

        assert!(result.is_ok())
    }

    #[test]
    fn can_generate_multi_file_module() {
        run_test(|context| {
            let Context { multi_gen, mod_root } = context;

            let single_file = false;

            multi_gen.clone().build().unwrap().write_to_module(&mod_root, single_file).unwrap();
            multi_gen
                .clone()
                .build()
                .unwrap()
                .ensure_consistent_module(&mod_root, single_file)
                .expect("Inconsistent bindings");
        })
    }

    #[test]
    fn can_generate_single_file_module() {
        run_test(|context| {
            let Context { multi_gen, mod_root } = context;

            let single_file = true;

            multi_gen.clone().build().unwrap().write_to_module(&mod_root, single_file).unwrap();
            multi_gen
                .clone()
                .build()
                .unwrap()
                .ensure_consistent_module(&mod_root, single_file)
                .expect("Inconsistent bindings");
        })
    }

    #[test]
    fn can_generate_multi_file_crate() {
        run_test(|context| {
            let Context { multi_gen, mod_root } = context;

            let single_file = false;
            let name = "a-name";
            let version = "290.3782.3";

            multi_gen
                .clone()
                .build()
                .unwrap()
                .write_to_crate(name, version, &mod_root, single_file)
                .unwrap();
            multi_gen
                .clone()
                .build()
                .unwrap()
                .ensure_consistent_crate(name, version, &mod_root, single_file, true)
                .expect("Inconsistent bindings");
        })
    }

    #[test]
    fn can_generate_single_file_crate() {
        run_test(|context| {
            let Context { multi_gen, mod_root } = context;

            let single_file = true;
            let name = "a-name";
            let version = "290.3782.3";

            multi_gen
                .clone()
                .build()
                .unwrap()
                .write_to_crate(name, version, &mod_root, single_file)
                .unwrap();
            multi_gen
                .clone()
                .build()
                .unwrap()
                .ensure_consistent_crate(name, version, &mod_root, single_file, true)
                .expect("Inconsistent bindings");
        })
    }

    #[test]
    fn can_detect_incosistent_multi_file_module() {
        run_test(|context| {
            let Context { multi_gen, mod_root } = context;

            let single_file = false;

            multi_gen.clone().build().unwrap().write_to_module(&mod_root, single_file).unwrap();

            let mut cloned = multi_gen.clone();
            cloned.push(
                Abigen::new(
                    "AdditionalContract",
                    r#"[
                        getValue() (uint256)
                    ]"#,
                )
                .unwrap(),
            );

            let result =
                cloned.build().unwrap().ensure_consistent_module(&mod_root, single_file).is_err();

            // ensure inconsistent bindings are detected
            assert!(result, "Inconsistent bindings wrongly approved");
        })
    }

    #[test]
    fn can_detect_incosistent_single_file_module() {
        run_test(|context| {
            let Context { multi_gen, mod_root } = context;

            let single_file = true;

            multi_gen.clone().build().unwrap().write_to_module(&mod_root, single_file).unwrap();

            let mut cloned = multi_gen.clone();
            cloned.push(
                Abigen::new(
                    "AdditionalContract",
                    r#"[
                        getValue() (uint256)
                    ]"#,
                )
                .unwrap(),
            );

            let result =
                cloned.build().unwrap().ensure_consistent_module(&mod_root, single_file).is_err();

            // ensure inconsistent bindings are detected
            assert!(result, "Inconsistent bindings wrongly approved");
        })
    }

    #[test]
    fn can_detect_incosistent_multi_file_crate() {
        run_test(|context| {
            let Context { multi_gen, mod_root } = context;

            let single_file = false;
            let name = "a-name";
            let version = "290.3782.3";

            multi_gen
                .clone()
                .build()
                .unwrap()
                .write_to_crate(name, version, &mod_root, single_file)
                .unwrap();

            let mut cloned = multi_gen.clone();
            cloned.push(
                Abigen::new(
                    "AdditionalContract",
                    r#"[
                            getValue() (uint256)
                        ]"#,
                )
                .unwrap(),
            );

            let result = cloned
                .build()
                .unwrap()
                .ensure_consistent_crate(name, version, &mod_root, single_file, true)
                .is_err();

            // ensure inconsistent bindings are detected
            assert!(result, "Inconsistent bindings wrongly approved");
        })
    }

    #[test]
    fn can_detect_inconsistent_single_file_crate() {
        run_test(|context| {
            let Context { multi_gen, mod_root } = context;

            let single_file = true;
            let name = "a-name";
            let version = "290.3782.3";

            multi_gen
                .clone()
                .build()
                .unwrap()
                .write_to_crate(name, version, &mod_root, single_file)
                .unwrap();

            let mut cloned = multi_gen.clone();
            cloned.push(
                Abigen::new(
                    "AdditionalContract",
                    r#"[
                            getValue() (uint256)
                        ]"#,
                )
                .unwrap(),
            );

            let result = cloned
                .build()
                .unwrap()
                .ensure_consistent_crate(name, version, &mod_root, single_file, true)
                .is_err();

            // ensure inconsistent bindings are detected
            assert!(result, "Inconsistent bindings wrongly approved");
        })
    }

    #[test]
    fn does_not_generate_shared_types_if_empty() {
        let gen = Abigen::new(
            "Greeter",
            r#"[
                        struct Inner {bool a;}
                        greet1() (uint256)
                        greet2(Inner inner) (string)
                    ]"#,
        )
        .unwrap();

        let tokens = MultiExpansion::new(vec![gen.expand().unwrap()]).expand_inplace().to_string();
        assert!(!tokens.contains("mod __shared_types"));
    }

    #[test]
    fn can_deduplicate_types() {
        let tmp = TempProject::dapptools().unwrap();

        tmp.add_source(
            "Greeter",
            r#"
// SPDX-License-Identifier: MIT
pragma solidity >=0.8.0;

struct Inner {
    bool a;
}

struct Stuff {
    Inner inner;
}

contract Greeter1 {

    function greet(Stuff calldata stuff) public pure returns (Stuff memory) {
        return stuff;
    }
}

contract Greeter2 {

    function greet(Stuff calldata stuff) public pure returns (Stuff memory) {
        return stuff;
    }
}
"#,
        )
        .unwrap();

        let _ = tmp.compile().unwrap();

        let gen = MultiAbigen::from_json_files(tmp.artifacts_path()).unwrap();
        let bindings = gen.build().unwrap();
        let single_file_dir = tmp.root().join("single_bindings");
        bindings.write_to_module(&single_file_dir, true).unwrap();

        let single_file_mod = single_file_dir.join("mod.rs");
        assert!(single_file_mod.exists());
        let content = fs::read_to_string(&single_file_mod).unwrap();
        assert!(content.contains("mod __shared_types"));
        assert!(content.contains("pub struct Inner"));
        assert!(content.contains("pub struct Stuff"));

        // multiple files
        let gen = MultiAbigen::from_json_files(tmp.artifacts_path()).unwrap();
        let bindings = gen.build().unwrap();
        let multi_file_dir = tmp.root().join("multi_bindings");
        bindings.write_to_module(&multi_file_dir, false).unwrap();
        let multi_file_mod = multi_file_dir.join("mod.rs");
        assert!(multi_file_mod.exists());
        let content = fs::read_to_string(&multi_file_mod).unwrap();
        assert!(content.contains("pub mod shared_types"));

        let greeter1 = multi_file_dir.join("greeter_1.rs");
        assert!(greeter1.exists());
        let content = fs::read_to_string(&greeter1).unwrap();
        assert!(!content.contains("pub struct Inner"));
        assert!(!content.contains("pub struct Stuff"));

        let greeter2 = multi_file_dir.join("greeter_2.rs");
        assert!(greeter2.exists());
        let content = fs::read_to_string(&greeter2).unwrap();
        assert!(!content.contains("pub struct Inner"));
        assert!(!content.contains("pub struct Stuff"));

        let shared_types = multi_file_dir.join("shared_types.rs");
        assert!(shared_types.exists());
        let content = fs::read_to_string(&shared_types).unwrap();
        assert!(content.contains("pub struct Inner"));
        assert!(content.contains("pub struct Stuff"));
    }

    #[test]
    fn can_sanitize_reserved_words() {
        let tmp = TempProject::dapptools().unwrap();

        tmp.add_source(
            "ReservedWords",
            r#"
// SPDX-License-Identifier: MIT
pragma solidity >=0.8.0;

contract Mod {
    function greet() public pure returns (uint256) {
        return 1;
    }
}

// from a gnosis contract
contract Enum {
    enum Operation {Call, DelegateCall}
}
"#,
        )
        .unwrap();

        let _ = tmp.compile().unwrap();

        let gen = MultiAbigen::from_json_files(tmp.artifacts_path()).unwrap();
        let bindings = gen.build().unwrap();
        let single_file_dir = tmp.root().join("single_bindings");
        bindings.write_to_module(&single_file_dir, true).unwrap();

        let single_file_mod = single_file_dir.join("mod.rs");
        assert!(single_file_mod.exists());
        let content = fs::read_to_string(&single_file_mod).unwrap();
        assert!(content.contains("pub mod mod_ {"));
        assert!(content.contains("pub mod enum_ {"));

        // multiple files
        let gen = MultiAbigen::from_json_files(tmp.artifacts_path()).unwrap();
        let bindings = gen.build().unwrap();
        let multi_file_dir = tmp.root().join("multi_bindings");
        bindings.write_to_module(&multi_file_dir, false).unwrap();
        let multi_file_mod = multi_file_dir.join("mod.rs");
        assert!(multi_file_mod.exists());
        let content = fs::read_to_string(&multi_file_mod).unwrap();
        assert!(content.contains("pub mod enum_;"));
        assert!(content.contains("pub mod mod_;"));

        let enum_ = multi_file_dir.join("enum_.rs");
        assert!(enum_.exists());
        let content = fs::read_to_string(&enum_).unwrap();
        assert!(content.contains("pub mod enum_ {"));

        let mod_ = multi_file_dir.join("mod_.rs");
        assert!(mod_.exists());
        let content = fs::read_to_string(&mod_).unwrap();
        assert!(content.contains("pub mod mod_ {"));
    }
}
