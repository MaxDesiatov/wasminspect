use super::address::*;
use super::executor::{eval_const_expr, invoke_func, WasmError};
use super::func::{DefinedFunctionInstance, FunctionInstance, HostFunctionInstance};
use super::global::GlobalInstance;
use super::host::HostValue;
use super::linker::LinkableCollection;
use super::memory::{self, MemoryInstance};
use super::module::{
    self, DefinedModuleInstance, HostExport, HostModuleInstance, ModuleIndex, ModuleInstance,
};
use super::table::{self, TableInstance};
use super::value::Value;
use parity_wasm::elements::{FunctionType, ValueType};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// Store
pub struct Store {
    funcs: LinkableCollection<FunctionInstance>,
    tables: LinkableCollection<Rc<RefCell<TableInstance>>>,
    mems: LinkableCollection<Rc<RefCell<MemoryInstance>>>,
    globals: LinkableCollection<Rc<RefCell<GlobalInstance>>>,
    modules: Vec<ModuleInstance>,
    module_index_by_name: HashMap<String, ModuleIndex>,

    embedded_contexts: HashMap<std::any::TypeId, Box<dyn std::any::Any>>,
}

impl Store {
    pub fn new() -> Self {
        Self {
            funcs: LinkableCollection::new(),
            tables: LinkableCollection::new(),
            mems: LinkableCollection::new(),
            globals: LinkableCollection::new(),
            modules: Vec::new(),
            module_index_by_name: HashMap::new(),
            embedded_contexts: HashMap::new(),
        }
    }

    pub fn func_global(&self, addr: ExecutableFuncAddr) -> &FunctionInstance {
        self.funcs.get_global(addr)
    }

    pub fn func(&self, addr: FuncAddr) -> Option<(&FunctionInstance, ExecutableFuncAddr)> {
        self.funcs.get(addr)
    }

    pub fn global(&self, addr: GlobalAddr) -> Rc<RefCell<GlobalInstance>> {
        self.globals.get(addr).unwrap().0.clone()
    }

    pub fn scan_global_by_name(
        &self,
        module_index: ModuleIndex,
        field: &str,
    ) -> Option<Rc<RefCell<GlobalInstance>>> {
        let module = self.module(module_index).defined().unwrap();
        let global_addr = module.exported_global(field.to_string()).ok().unwrap();
        global_addr.map(|addr| self.global(addr))
    }

    pub fn table(&self, addr: TableAddr) -> Rc<RefCell<TableInstance>> {
        self.tables.get(addr).unwrap().0.clone()
    }

    pub fn memory(&self, addr: MemoryAddr) -> Rc<RefCell<MemoryInstance>> {
        self.mems.get(addr).unwrap().0.clone()
    }

    pub fn memory_count(&self, addr: ModuleIndex) -> usize {
        self.mems.items(addr).map(|c| c.len()).unwrap_or(0)
    }

    pub fn module(&self, module_index: ModuleIndex) -> &ModuleInstance {
        &self.modules[module_index.0 as usize]
    }

    pub fn module_by_name(&self, name: String) -> &ModuleInstance {
        if let Some(index) = self.module_index_by_name.get(&name) {
            self.module(*index)
        } else {
            panic!("Module {} was not loaded", name);
        }
    }

    pub fn register_name(&mut self, name: String, module_index: ModuleIndex) {
        self.module_index_by_name.insert(name, module_index);
    }
}

impl Store {
    pub fn load_host_module(&mut self, name: String, module: HashMap<String, HostValue>) {
        let module_index = ModuleIndex(self.modules.len() as u32);
        let mut values = HashMap::new();
        for (field, entry) in module {
            match entry {
                HostValue::Func(f) => {
                    let instance =
                        HostFunctionInstance::new(f.ty().clone(), name.clone(), field.clone(), f);
                    let addr = self.funcs.push_global(FunctionInstance::Host(instance));
                    values.insert(field, HostExport::Func(addr));
                }
                HostValue::Global(g) => {
                    let addr = self.globals.push_global(g);
                    values.insert(field, HostExport::Global(addr));
                }
                HostValue::Table(t) => {
                    let addr = self.tables.push_global(t);
                    values.insert(field, HostExport::Table(addr));
                }
                HostValue::Mem(m) => {
                    let addr = self.mems.push_global(m);
                    values.insert(field, HostExport::Mem(addr));
                }
            }
        }
        let instance = HostModuleInstance::new(values);
        self.modules.push(ModuleInstance::Host(instance));
        self.module_index_by_name.insert(name, module_index);
    }

    pub fn add_embed_context<T: std::any::Any>(&mut self, ctx: Box<T>) {
        let type_id = std::any::TypeId::of::<T>();
        self.embedded_contexts.insert(type_id, ctx);
    }

    pub fn get_embed_context<T: std::any::Any>(&self) -> Option<&T> {
        let type_id = std::any::TypeId::of::<T>();
        self.embedded_contexts
            .get(&type_id)
            .map(|v| v.downcast_ref::<T>().unwrap())
    }
}

pub enum Error {
    InvalidElementSegments(table::Error),
    InvalidDataSegments(memory::Error),
    InvalidHostImport(module::HostModuleError),
    InvalidImport(module::DefinedModuleError),
    UnknownType(/* type index: */ u32),
    UndefinedFunction(/* module: */ String, /* name: */ String),
    UndefinedMemory(String, String),
    UndefinedTable(String, String),
    UndefinedGlobal(String, String),
    FailedEntryFunction(WasmError),
    IncompatibleImportFuncType(String, FunctionType, FunctionType),
    IncompatibleImportGlobalType(ValueType, ValueType),
    IncompatibleImportGlobalMutability,
    IncompatibleImportTableType,
    IncompatibleImportMemoryType,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidElementSegments(err) => {
                write!(f, "elements segment does not fit: {:?}", err)
            }
            Self::InvalidDataSegments(err) => write!(f, "data segment does not fit: {}", err),
            Self::InvalidHostImport(err) => write!(f, "invalid host import: {}", err),
            Self::InvalidImport(err) => write!(f, "invalid import: {}", err),
            Self::UnknownType(idx) => write!(f, "Unknown type index used: {:?}", idx),
            Self::UndefinedFunction(module, name) => write!(
                f,
                "unknown import: Undefined function \"{}\" in \"{}\"",
                name, module
            ),
            Self::UndefinedMemory(module, name) => write!(
                f,
                "unknown import: Undefined memory \"{}\" in \"{}\"",
                name, module
            ),
            Self::UndefinedTable(module, name) => write!(
                f,
                "unknown import: Undefined table \"{}\" in \"{}\"",
                name, module
            ),
            Self::UndefinedGlobal(module, name) => write!(
                f,
                "unknown import: Undefined global \"{}\" in \"{}\"",
                name, module
            ),
            Self::FailedEntryFunction(e) => write!(f, "{}", e),
            Self::IncompatibleImportFuncType(name, expected, actual) => write!(
                f,
                "incompatible import type, \"{}\" expected {:?} but got {:?}",
                name, expected, actual
            ),
            Self::IncompatibleImportGlobalType(expected, actual) => write!(
                f,
                "incompatible import type, expected {:?} but got {:?}",
                expected, actual
            ),
            Self::IncompatibleImportGlobalMutability => write!(f, "incompatible import type"),
            Self::IncompatibleImportTableType => write!(f, "incompatible import type"),
            Self::IncompatibleImportMemoryType => write!(f, "incompatible import type"),
        }
    }
}

type Result<T> = std::result::Result<T, Error>;

impl Store {
    fn load_parity_module_internal(
        &mut self,
        name: Option<String>,
        parity_module: parity_wasm::elements::Module,
        module_index: ModuleIndex,
    ) -> Result<ModuleIndex> {
        let types = Self::get_types(&parity_module);
        let elem_segs = Self::get_element_segments(&parity_module);
        let data_segs = Self::get_data_segments(&parity_module);

        self.load_imports(&parity_module, module_index, types)?;
        self.load_functions(&parity_module, module_index, types)?;

        self.load_globals(&parity_module, module_index);
        self.load_tables(&parity_module, module_index, elem_segs)?;
        self.load_mems(&parity_module, module_index, data_segs)?;

        let types = types.iter().map(|ty| ty.clone()).collect();

        let instance =
            DefinedModuleInstance::new_from_parity_module(parity_module, module_index, types);
        self.modules.push(ModuleInstance::Defined(instance));
        if let Some(name) = name {
            self.module_index_by_name.insert(name, module_index);
        }

        Ok(module_index)
    }
    pub fn load_parity_module(
        &mut self,
        name: Option<String>,
        parity_module: parity_wasm::elements::Module,
    ) -> Result<ModuleIndex> {
        let module_index = ModuleIndex(self.modules.len() as u32);
        let start_section = parity_module.start_section().clone();

        let result: Result<ModuleIndex> =
            self.load_parity_module_internal(name.clone(), parity_module, module_index);
        if let Some(start_section) = start_section {
            let func_addr = FuncAddr::new_unsafe(module_index, start_section as usize);
            // TODO: Handle result
            invoke_func(func_addr, vec![], self).map_err(Error::FailedEntryFunction)?;
        }
        match result {
            Ok(ok) => Ok(ok),
            Err(err) => {
                // If fail, cleanup states
                self.funcs.remove_module(&module_index);
                self.tables.remove_module(&module_index);
                self.mems.remove_module(&module_index);
                self.globals.remove_module(&module_index);
                let module_index = module_index.0 as usize;
                if module_index < self.modules.len() {
                    self.modules.remove(module_index);
                }
                if let Some(ref name) = name.clone() {
                    self.module_index_by_name.remove(name);
                }
                Err(err)
            }
        }
    }

    fn get_types(parity_module: &parity_wasm::elements::Module) -> &[parity_wasm::elements::Type] {
        return parity_module
            .type_section()
            .map(|sec| sec.types())
            .unwrap_or_default();
    }

    fn get_element_segments(
        parity_module: &parity_wasm::elements::Module,
    ) -> HashMap<usize, Vec<&parity_wasm::elements::ElementSegment>> {
        let segments = parity_module
            .elements_section()
            .map(|sec| sec.entries())
            .unwrap_or_default();
        let mut result = HashMap::new();
        for seg in segments {
            result
                .entry(seg.index() as usize)
                .or_insert(Vec::new())
                .push(seg);
        }
        result
    }

    fn get_data_segments(
        parity_module: &parity_wasm::elements::Module,
    ) -> HashMap<usize, Vec<&parity_wasm::elements::DataSegment>> {
        let segments = parity_module
            .data_section()
            .map(|sec| sec.entries())
            .unwrap_or_default();

        let mut result = HashMap::new();
        for seg in segments {
            result
                .entry(seg.index() as usize)
                .or_insert(Vec::new())
                .push(seg);
        }
        result
    }

    fn load_imports(
        &mut self,
        parity_module: &parity_wasm::elements::Module,
        module_index: ModuleIndex,
        types: &[parity_wasm::elements::Type],
    ) -> Result<()> {
        let imports = parity_module
            .import_section()
            .map(|sec| sec.entries())
            .unwrap_or_default();
        for import in imports {
            match import.external() {
                parity_wasm::elements::External::Function(type_index) => {
                    self.load_import_function(module_index, import, *type_index as usize, &types)?;
                }
                parity_wasm::elements::External::Memory(memory_ty) => {
                    self.load_import_memory(module_index, import, *memory_ty)?;
                }
                parity_wasm::elements::External::Table(table_ty) => {
                    self.load_import_table(module_index, import, *table_ty)?;
                }
                parity_wasm::elements::External::Global(global_ty) => {
                    self.load_import_global(module_index, import, *global_ty)?;
                }
            }
        }
        Ok(())
    }

    fn load_import_function(
        &mut self,
        module_index: ModuleIndex,
        import: &parity_wasm::elements::ImportEntry,
        type_index: usize,
        types: &[parity_wasm::elements::Type],
    ) -> Result<()> {
        let func_ty = {
            let ty = types
                .get(type_index)
                .ok_or(Error::UnknownType(type_index as u32))?
                .clone();
            match ty {
                parity_wasm::elements::Type::Function(ty) => ty,
            }
        };
        let name = import.field().to_string();
        let module = self.module_by_name(import.module().to_string());
        let err = || {
            Error::UndefinedFunction(
                import.module().clone().to_string(),
                import.field().clone().to_string(),
            )
        };
        let exec_addr = match module {
            ModuleInstance::Defined(defined) => {
                let func_addr = defined
                    .exported_func(name)
                    .map_err(Error::InvalidImport)?
                    .ok_or_else(err)?;
                self.funcs.resolve(func_addr).ok_or_else(err)?.clone()
            }
            ModuleInstance::Host(host) => *host
                .func_by_name(import.field().to_string())
                .map_err(Error::InvalidHostImport)?
                .ok_or_else(err)?,
        };
        let actual_func_ty = self.funcs.get_global(exec_addr).ty();
        // Validation
        if *actual_func_ty != func_ty {
            return Err(Error::IncompatibleImportFuncType(
                import.field().to_string(),
                func_ty,
                actual_func_ty.clone(),
            ));
        }
        self.funcs.link(exec_addr, module_index);
        Ok(())
    }

    fn load_import_memory(
        &mut self,
        module_index: ModuleIndex,
        import: &parity_wasm::elements::ImportEntry,
        memory_ty: parity_wasm::elements::MemoryType,
    ) -> Result<()> {
        let err = || {
            Error::UndefinedMemory(
                import.module().clone().to_string(),
                import.field().clone().to_string(),
            )
        };
        let name = import.field().to_string();
        let module = self.module_by_name(import.module().to_string());
        let resolved_addr = match module {
            ModuleInstance::Defined(defined) => {
                let addr = defined
                    .exported_memory(name.clone())
                    .map_err(Error::InvalidImport)?
                    .ok_or(err())?
                    .clone();
                self.mems.resolve(addr).ok_or_else(err)?.clone()
            }
            ModuleInstance::Host(host) => *host
                .memory_by_name(name.clone())
                .map_err(Error::InvalidHostImport)?
                .ok_or(err())?,
        };

        // Validation
        {
            let memory = self.mems.get_global(resolved_addr);
            if memory.borrow().initial < memory_ty.limits().initial() as usize {
                return Err(Error::IncompatibleImportMemoryType);
            }
            match (memory.borrow().max, memory_ty.limits().maximum()) {
                (Some(found), Some(expected)) => {
                    if found > expected as usize {
                        return Err(Error::IncompatibleImportMemoryType);
                    }
                }
                (None, Some(_)) => return Err(Error::IncompatibleImportMemoryType),
                _ => (),
            }
        }
        self.mems.link(resolved_addr, module_index);
        Ok(())
    }

    fn load_import_table(
        &mut self,
        module_index: ModuleIndex,
        import: &parity_wasm::elements::ImportEntry,
        table_ty: parity_wasm::elements::TableType,
    ) -> Result<()> {
        let name = import.field().to_string();
        let module = self.module_by_name(import.module().to_string());
        let err = || {
            Error::UndefinedTable(
                import.module().clone().to_string(),
                import.field().clone().to_string(),
            )
        };
        let resolved_addr = match module {
            ModuleInstance::Defined(defined) => {
                let addr = defined
                    .exported_table(name.clone())
                    .map_err(Error::InvalidImport)?
                    .ok_or_else(err)?;
                self.tables.resolve(addr).ok_or_else(err)?.clone()
            }
            ModuleInstance::Host(host) => host
                .table_by_name(name.clone())
                .map_err(Error::InvalidHostImport)?
                .ok_or_else(err)?
                .clone(),
        };
        let found = self.tables.get_global(resolved_addr);
        // Validation
        {
            if found.borrow().initial < table_ty.limits().initial() as usize {
                return Err(Error::IncompatibleImportTableType);
            }
            match (found.clone().borrow().max, table_ty.limits().maximum()) {
                (Some(found), Some(expected)) => {
                    if found > expected as usize {
                        return Err(Error::IncompatibleImportTableType);
                    }
                }
                (None, Some(_)) => return Err(Error::IncompatibleImportTableType),
                _ => (),
            }
        }

        self.tables.link(resolved_addr, module_index);
        Ok(())
    }

    fn load_import_global(
        &mut self,
        module_index: ModuleIndex,
        import: &parity_wasm::elements::ImportEntry,
        global_ty: parity_wasm::elements::GlobalType,
    ) -> Result<()> {
        let name = import.field().to_string();
        let module = self.module_by_name(import.module().to_string());
        let err = || {
            Error::UndefinedGlobal(
                import.module().clone().to_string(),
                import.field().clone().to_string(),
            )
        };
        let resolved_addr = match module {
            ModuleInstance::Defined(defined) => {
                let addr = defined
                    .exported_global(name)
                    .map_err(Error::InvalidImport)?
                    .ok_or(err())?;
                self.globals.resolve(addr).ok_or_else(err)?.clone()
            }
            ModuleInstance::Host(host) => host
                .global_by_name(name)
                .map_err(Error::InvalidHostImport)
                .and_then(|f| f.ok_or(err()))?
                .clone(),
        };
        // Validation
        {
            let actual_global = self.globals.get_global(resolved_addr);
            let actual_global_ty = actual_global.borrow().ty().content_type().clone();
            let expected_global_ty = global_ty.content_type().clone();
            if actual_global.borrow().is_mutable() != global_ty.is_mutable() {
                return Err(Error::IncompatibleImportGlobalMutability);
            }
            if actual_global_ty != expected_global_ty {
                return Err(Error::IncompatibleImportGlobalType(
                    actual_global_ty,
                    expected_global_ty,
                ));
            }
        };
        self.globals.link(resolved_addr, module_index);
        Ok(())
    }

    fn load_functions(
        &mut self,
        parity_module: &parity_wasm::elements::Module,
        module_index: ModuleIndex,
        types: &[parity_wasm::elements::Type],
    ) -> Result<Vec<FuncAddr>> {
        let functions = parity_module
            .function_section()
            .map(|sec| sec.entries())
            .unwrap_or_default();
        let bodies = parity_module
            .code_section()
            .map(|sec| sec.bodies())
            .unwrap_or_default();
        let mut func_addrs = Vec::new();
        for (func, body) in functions.into_iter().zip(bodies) {
            let parity_wasm::elements::Type::Function(func_type) = types
                .get(func.type_ref() as usize)
                .ok_or(Error::UnknownType(func.type_ref()))?
                .clone();
            let defined = DefinedFunctionInstance::new(
                format!("<module defined func #{}>", self.funcs.len()),
                func_type,
                module_index,
                body.clone(),
            );
            let instance = FunctionInstance::Defined(defined);
            let func_addr = self.funcs.push(module_index, instance);
            func_addrs.push(func_addr);
        }
        Ok(func_addrs)
    }

    fn load_globals(
        &mut self,
        parity_module: &parity_wasm::elements::Module,
        module_index: ModuleIndex,
    ) -> Vec<GlobalAddr> {
        let globals = parity_module
            .global_section()
            .map(|sec| sec.entries())
            .unwrap_or_default();
        let mut global_addrs = Vec::new();
        for entry in globals {
            let value = eval_const_expr(entry.init_expr(), &self, module_index);
            let instance = GlobalInstance::new(value, entry.global_type().clone());
            let addr = self
                .globals
                .push(module_index, Rc::new(RefCell::new(instance)));
            global_addrs.push(addr);
        }
        global_addrs
    }

    fn load_tables(
        &mut self,
        parity_module: &parity_wasm::elements::Module,
        module_index: ModuleIndex,
        element_segments: HashMap<usize, Vec<&parity_wasm::elements::ElementSegment>>,
    ) -> Result<Vec<TableAddr>> {
        let tables = parity_module
            .table_section()
            .map(|sec| sec.entries())
            .unwrap_or_default();
        let mut table_addrs = Vec::new();
        if tables.is_empty() && self.tables.is_empty(module_index) {
            return Ok(table_addrs);
        }
        for entry in tables.iter() {
            match entry.elem_type() {
                parity_wasm::elements::TableElementType::AnyFunc => {
                    let instance = TableInstance::new(
                        entry.limits().initial() as usize,
                        entry.limits().maximum().map(|mx| mx as usize),
                    );
                    let addr = self
                        .tables
                        .push(module_index, Rc::new(RefCell::new(instance)));
                    table_addrs.push(addr);
                }
            }
        }
        for (index, table_addr) in self.tables.items(module_index).unwrap().iter().enumerate() {
            let segs = match element_segments.get(&index) {
                Some(segs) => segs,
                None => continue,
            };
            for seg in segs {
                let offset = match seg
                    .offset()
                    .as_ref()
                    .map(|e| eval_const_expr(&e, self, module_index))
                    .unwrap()
                {
                    Value::I32(v) => v,
                    _ => panic!(),
                };
                let data = seg
                    .members()
                    .iter()
                    .map(|func_index| FuncAddr::new_unsafe(module_index, *func_index as usize))
                    .collect();
                let table = self.tables.get_global(*table_addr);
                table
                    .borrow_mut()
                    .initialize(offset as usize, data)
                    .map_err(Error::InvalidElementSegments)?;
            }
        }
        Ok(table_addrs)
    }

    fn load_mems(
        &mut self,
        parity_module: &parity_wasm::elements::Module,
        module_index: ModuleIndex,
        data_segments: HashMap<usize, Vec<&parity_wasm::elements::DataSegment>>,
    ) -> Result<Vec<MemoryAddr>> {
        let mem_sec = parity_module
            .memory_section()
            .map(|sec| sec.entries())
            .unwrap_or_default();
        let mut mem_addrs = Vec::new();
        if mem_sec.is_empty() && self.mems.is_empty(module_index) {
            return Ok(mem_addrs);
        }
        for entry in mem_sec.iter() {
            let instance = MemoryInstance::new(
                entry.limits().initial() as usize,
                entry.limits().maximum().map(|mx| mx as usize),
            );
            let addr = self
                .mems
                .push(module_index, Rc::new(RefCell::new(instance)));
            mem_addrs.push(addr);
        }

        let mut offsets_and_value = Vec::new();
        for (index, mem_addr) in self.mems.items(module_index).unwrap().iter().enumerate() {
            if let Some(segs) = data_segments.get(&index) {
                for seg in segs {
                    let offset = match seg
                        .offset()
                        .as_ref()
                        .map(|e| eval_const_expr(&e, self, module_index))
                        .unwrap()
                    {
                        Value::I32(v) => v,
                        _ => panic!(),
                    };
                    let mem = self.mems.get_global(*mem_addr);
                    mem.borrow()
                        .validate_region(offset as usize, seg.value().len())
                        .map_err(Error::InvalidDataSegments)?;
                    offsets_and_value.push((mem, offset, seg.value()));
                }
            }
        }

        for (mem, offset, value) in offsets_and_value {
            mem.borrow_mut()
                .store(offset as usize, value)
                .map_err(Error::InvalidDataSegments)?;
        }
        Ok(mem_addrs)
    }
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "")
    }
}