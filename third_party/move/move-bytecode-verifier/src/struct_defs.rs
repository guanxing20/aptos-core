// Copyright (c) The Diem Core Contributors
// Copyright (c) The Move Contributors
// SPDX-License-Identifier: Apache-2.0

//! This module provides a checker for verifying that struct definitions in a module are not
//! recursive. Since the module dependency graph is acylic by construction, applying this checker to
//! each module in isolation guarantees that there is no structural recursion globally.
use move_binary_format::{
    access::ModuleAccess,
    errors::{verification_error, Location, PartialVMError, PartialVMResult, VMResult},
    file_format::{
        CompiledModule, SignatureToken, StructDefinitionIndex, StructHandleIndex, TableIndex,
        VariantIndex,
    },
    internals::ModuleIndex,
    views::StructDefinitionView,
    IndexKind,
};
use move_core_types::vm_status::StatusCode;
use petgraph::{algo::toposort, graphmap::DiGraphMap};
use std::collections::{BTreeMap, BTreeSet};

pub struct RecursiveStructDefChecker<'a> {
    module: &'a CompiledModule,
}

impl<'a> RecursiveStructDefChecker<'a> {
    pub fn verify_module(module: &'a CompiledModule) -> VMResult<()> {
        Self::verify_module_impl(module).map_err(|e| e.finish(Location::Module(module.self_id())))
    }

    fn verify_module_impl(module: &'a CompiledModule) -> PartialVMResult<()> {
        let checker = Self { module };
        let graph = StructDefGraphBuilder::new(checker.module).build()?;

        // toposort is iterative while petgraph::algo::is_cyclic_directed is recursive. Prefer
        // the iterative solution here as this code may be dealing with untrusted data.
        match toposort(&graph, None) {
            Ok(_) => Ok(()),
            Err(cycle) => Err(verification_error(
                StatusCode::RECURSIVE_STRUCT_DEFINITION,
                IndexKind::StructDefinition,
                cycle.node_id().into_index() as TableIndex,
            )),
        }
    }
}

/// Given a module, build a graph of struct definitions. This is useful when figuring out whether
/// the struct definitions in module form a cycle.
struct StructDefGraphBuilder<'a> {
    module: &'a CompiledModule,
    /// Used to follow field definitions' signatures' struct handles to their struct definitions.
    handle_to_def: BTreeMap<StructHandleIndex, StructDefinitionIndex>,
}

impl<'a> StructDefGraphBuilder<'a> {
    fn new(module: &'a CompiledModule) -> Self {
        let mut handle_to_def = BTreeMap::new();
        // the mapping from struct definitions to struct handles is already checked to be 1-1 by
        // DuplicationChecker
        for (idx, struct_def) in module.struct_defs().iter().enumerate() {
            let sh_idx = struct_def.struct_handle;
            handle_to_def.insert(sh_idx, StructDefinitionIndex(idx as TableIndex));
        }

        Self {
            module,
            handle_to_def,
        }
    }

    fn build(self) -> PartialVMResult<DiGraphMap<StructDefinitionIndex, ()>> {
        let mut neighbors = BTreeMap::new();
        for idx in 0..self.module.struct_defs().len() {
            let sd_idx = StructDefinitionIndex::new(idx as TableIndex);
            self.add_struct_defs(&mut neighbors, sd_idx)?
        }

        let edges = neighbors
            .into_iter()
            .flat_map(|(parent, children)| children.into_iter().map(move |child| (parent, child)));
        Ok(DiGraphMap::from_edges(edges))
    }

    fn add_struct_defs(
        &self,
        neighbors: &mut BTreeMap<StructDefinitionIndex, BTreeSet<StructDefinitionIndex>>,
        idx: StructDefinitionIndex,
    ) -> PartialVMResult<()> {
        let struct_def = self.module.struct_def_at(idx);
        let struct_def = StructDefinitionView::new(self.module, struct_def);
        let variant_count = struct_def.variant_count();
        if variant_count > 0 {
            for i in 0..variant_count {
                for field in struct_def.fields_optional_variant(Some(i as VariantIndex)) {
                    self.add_signature_token(neighbors, idx, field.signature_token(), false)?
                }
            }
        } else {
            for field in struct_def.fields_optional_variant(None) {
                self.add_signature_token(neighbors, idx, field.signature_token(), false)?
            }
        }
        Ok(())
    }

    #[allow(clippy::unit_arg)]
    fn add_signature_token(
        &self,
        neighbors: &mut BTreeMap<StructDefinitionIndex, BTreeSet<StructDefinitionIndex>>,
        cur_idx: StructDefinitionIndex,
        token: &SignatureToken,
        ref_allowed: bool,
    ) -> PartialVMResult<()> {
        use SignatureToken as T;
        Ok(match token {
            T::Bool
            | T::U8
            | T::U16
            | T::U32
            | T::U64
            | T::U128
            | T::U256
            | T::Address
            | T::Signer
            | T::TypeParameter(_) => (),
            T::Reference(t) | T::MutableReference(t) => {
                if ref_allowed {
                    self.add_signature_token(neighbors, cur_idx, t, false)?
                } else {
                    return Err(
                        PartialVMError::new(StatusCode::UNKNOWN_INVARIANT_VIOLATION_ERROR)
                            .with_message(
                                "Reference field when checking recursive structs".to_owned(),
                            ),
                    );
                }
            },
            T::Vector(inner) => self.add_signature_token(neighbors, cur_idx, inner, false)?,
            T::Function(args, result, _) => {
                for t in args.iter().chain(result) {
                    // Function arguments and results can have references at outer
                    // position, so set ref_allowed to true
                    self.add_signature_token(neighbors, cur_idx, t, true)?
                }
            },
            T::Struct(sh_idx) => {
                if let Some(struct_def_idx) = self.handle_to_def.get(sh_idx) {
                    neighbors
                        .entry(cur_idx)
                        .or_default()
                        .insert(*struct_def_idx);
                }
            },
            T::StructInstantiation(sh_idx, inners) => {
                if let Some(struct_def_idx) = self.handle_to_def.get(sh_idx) {
                    neighbors
                        .entry(cur_idx)
                        .or_default()
                        .insert(*struct_def_idx);
                }
                for t in inners {
                    self.add_signature_token(neighbors, cur_idx, t, false)?
                }
            },
        })
    }
}
