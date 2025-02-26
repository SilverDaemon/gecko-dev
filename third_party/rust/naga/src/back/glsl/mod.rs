/*!
Backend for [GLSL][glsl] (OpenGL Shading Language).

The main structure is [`Writer`], it maintains internal state that is used
to output a [`Module`](crate::Module) into glsl

# Supported versions
### Core
- 330
- 400
- 410
- 420
- 430
- 450
- 460

### ES
- 300
- 310

[glsl]: https://www.khronos.org/registry/OpenGL/index_gl.php
*/

// GLSL is mostly a superset of C but it also removes some parts of it this is a list of relevant
// aspects for this backend.
//
// The most notable change is the introduction of the version preprocessor directive that must
// always be the first line of a glsl file and is written as
// `#version number profile`
// `number` is the version itself (i.e. 300) and `profile` is the
// shader profile we only support "core" and "es", the former is used in desktop applications and
// the later is used in embedded contexts, mobile devices and browsers. Each one as it's own
// versions (at the time of writing this the latest version for "core" is 460 and for "es" is 320)
//
// Other important preprocessor addition is the extension directive which is written as
// `#extension name: behaviour`
// Extensions provide increased features in a plugin fashion but they aren't required to be
// supported hence why they are called extensions, that's why `behaviour` is used it specifies
// whether the extension is strictly required or if it should only be enabled if needed. In our case
// when we use extensions we set behaviour to `require` always.
//
// The only thing that glsl removes that makes a difference are pointers.
//
// Additions that are relevant for the backend are the discard keyword, the introduction of
// vector, matrices, samplers, image types and functions that provide common shader operations

pub use features::Features;

use crate::{
    back,
    proc::{self, NameKey},
    valid, Handle, ShaderStage, TypeInner,
};
use features::FeaturesManager;
use std::{
    cmp::Ordering,
    fmt,
    fmt::{Error as FmtError, Write},
};
use thiserror::Error;

/// Contains the features related code and the features querying method
mod features;
/// Contains a constant with a slice of all the reserved keywords RESERVED_KEYWORDS
mod keywords;

/// List of supported `core` GLSL versions.
pub const SUPPORTED_CORE_VERSIONS: &[u16] = &[330, 400, 410, 420, 430, 440, 450];
/// List of supported `es` GLSL versions.
pub const SUPPORTED_ES_VERSIONS: &[u16] = &[300, 310, 320];

/// Mapping between resources and bindings.
pub type BindingMap = std::collections::BTreeMap<crate::ResourceBinding, u8>;

impl crate::AtomicFunction {
    const fn to_glsl(self) -> &'static str {
        match self {
            Self::Add | Self::Subtract => "Add",
            Self::And => "And",
            Self::InclusiveOr => "Or",
            Self::ExclusiveOr => "Xor",
            Self::Min => "Min",
            Self::Max => "Max",
            Self::Exchange { compare: None } => "Exchange",
            Self::Exchange { compare: Some(_) } => "", //TODO
        }
    }
}

impl crate::AddressSpace {
    const fn is_buffer(&self) -> bool {
        match *self {
            crate::AddressSpace::Uniform | crate::AddressSpace::Storage { .. } => true,
            _ => false,
        }
    }

    /// Whether a variable with this address space can be initialized
    const fn initializable(&self) -> bool {
        match *self {
            crate::AddressSpace::Function | crate::AddressSpace::Private => true,
            crate::AddressSpace::WorkGroup
            | crate::AddressSpace::Uniform
            | crate::AddressSpace::Storage { .. }
            | crate::AddressSpace::Handle
            | crate::AddressSpace::PushConstant => false,
        }
    }
}

/// A GLSL version.
#[derive(Debug, Copy, Clone, PartialEq)]
#[cfg_attr(feature = "serialize", derive(serde::Serialize))]
#[cfg_attr(feature = "deserialize", derive(serde::Deserialize))]
pub enum Version {
    /// `core` GLSL.
    Desktop(u16),
    /// `es` GLSL.
    Embedded(u16),
}

impl Version {
    /// Returns true if self is `Version::Embedded` (i.e. is a es version)
    const fn is_es(&self) -> bool {
        match *self {
            Version::Desktop(_) => false,
            Version::Embedded(_) => true,
        }
    }

    /// Checks the list of currently supported versions and returns true if it contains the
    /// specified version
    ///
    /// # Notes
    /// As an invalid version number will never be added to the supported version list
    /// so this also checks for version validity
    fn is_supported(&self) -> bool {
        match *self {
            Version::Desktop(v) => SUPPORTED_CORE_VERSIONS.contains(&v),
            Version::Embedded(v) => SUPPORTED_ES_VERSIONS.contains(&v),
        }
    }

    /// Checks if the version supports all of the explicit layouts:
    /// - `location=` qualifiers for bindings
    /// - `binding=` qualifiers for resources
    ///
    /// Note: `location=` for vertex inputs and fragment outputs is supported
    /// unconditionally for GLES 300.
    fn supports_explicit_locations(&self) -> bool {
        *self >= Version::Embedded(310) || *self >= Version::Desktop(410)
    }

    fn supports_early_depth_test(&self) -> bool {
        *self >= Version::Desktop(130) || *self >= Version::Embedded(310)
    }

    fn supports_std430_layout(&self) -> bool {
        *self >= Version::Desktop(430) || *self >= Version::Embedded(310)
    }

    fn supports_fma_function(&self) -> bool {
        *self >= Version::Desktop(400) || *self >= Version::Embedded(310)
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        match (*self, *other) {
            (Version::Desktop(x), Version::Desktop(y)) => Some(x.cmp(&y)),
            (Version::Embedded(x), Version::Embedded(y)) => Some(x.cmp(&y)),
            _ => None,
        }
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Version::Desktop(v) => write!(f, "{} core", v),
            Version::Embedded(v) => write!(f, "{} es", v),
        }
    }
}

bitflags::bitflags! {
    /// Configuration flags for the [`Writer`].
    #[cfg_attr(feature = "serialize", derive(serde::Serialize))]
    #[cfg_attr(feature = "deserialize", derive(serde::Deserialize))]
    pub struct WriterFlags: u32 {
        /// Flip output Y and extend Z from (0, 1) to (-1, 1).
        const ADJUST_COORDINATE_SPACE = 0x1;
        /// Supports GL_EXT_texture_shadow_lod on the host, which provides
        /// additional functions on shadows and arrays of shadows.
        const TEXTURE_SHADOW_LOD = 0x2;
    }
}

/// Configuration used in the [`Writer`].
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serialize", derive(serde::Serialize))]
#[cfg_attr(feature = "deserialize", derive(serde::Deserialize))]
pub struct Options {
    /// The GLSL version to be used.
    pub version: Version,
    /// Configuration flags for the [`Writer`].
    pub writer_flags: WriterFlags,
    /// Map of resources association to binding locations.
    pub binding_map: BindingMap,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            version: Version::Embedded(310),
            writer_flags: WriterFlags::ADJUST_COORDINATE_SPACE,
            binding_map: BindingMap::default(),
        }
    }
}

/// A subset of options meant to be changed per pipeline.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serialize", derive(serde::Serialize))]
#[cfg_attr(feature = "deserialize", derive(serde::Deserialize))]
pub struct PipelineOptions {
    /// The stage of the entry point.
    pub shader_stage: ShaderStage,
    /// The name of the entry point.
    ///
    /// If no entry point that matches is found while creating a [`Writer`], a error will be thrown.
    pub entry_point: String,
}

/// Reflection info for texture mappings and uniforms.
pub struct ReflectionInfo {
    /// Mapping between texture names and variables/samplers.
    pub texture_mapping: crate::FastHashMap<String, TextureMapping>,
    /// Mapping between uniform variables and names.
    pub uniforms: crate::FastHashMap<Handle<crate::GlobalVariable>, String>,
}

/// Mapping between a texture and its sampler, if it exists.
///
/// GLSL pre-Vulkan has no concept of separate textures and samplers. Instead, everything is a
/// `gsamplerN` where `g` is the scalar type and `N` is the dimension. But naga uses separate textures
/// and samplers in the IR, so the backend produces a [`FastHashMap`](crate::FastHashMap) with the texture name
/// as a key and a [`TextureMapping`] as a value. This way, the user knows where to bind.
///
/// [`Storage`](crate::ImageClass::Storage) images produce `gimageN` and don't have an associated sampler,
/// so the [`sampler`](Self::sampler) field will be [`None`].
#[derive(Debug, Clone)]
pub struct TextureMapping {
    /// Handle to the image global variable.
    pub texture: Handle<crate::GlobalVariable>,
    /// Handle to the associated sampler global variable, if it exists.
    pub sampler: Option<Handle<crate::GlobalVariable>>,
}

/// Helper structure that generates a number
#[derive(Default)]
struct IdGenerator(u32);

impl IdGenerator {
    /// Generates a number that's guaranteed to be unique for this `IdGenerator`
    fn generate(&mut self) -> u32 {
        // It's just an increasing number but it does the job
        let ret = self.0;
        self.0 += 1;
        ret
    }
}

/// Helper wrapper used to get a name for a varying
///
/// Varying have different naming schemes depending on their binding:
/// - Varyings with builtin bindings get the from [`glsl_built_in`](glsl_built_in).
/// - Varyings with location bindings are named `_S_location_X` where `S` is a
///   prefix identifying which pipeline stage the varying connects, and `X` is
///   the location.
struct VaryingName<'a> {
    binding: &'a crate::Binding,
    stage: ShaderStage,
    output: bool,
}
impl fmt::Display for VaryingName<'_> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self.binding {
            crate::Binding::Location { location, .. } => {
                let prefix = match (self.stage, self.output) {
                    (ShaderStage::Compute, _) => unreachable!(),
                    // pipeline to vertex
                    (ShaderStage::Vertex, false) => "p2vs",
                    // vertex to fragment
                    (ShaderStage::Vertex, true) | (ShaderStage::Fragment, false) => "vs2fs",
                    // fragment to pipeline
                    (ShaderStage::Fragment, true) => "fs2p",
                };
                write!(f, "_{}_location{}", prefix, location,)
            }
            crate::Binding::BuiltIn(built_in) => {
                write!(f, "{}", glsl_built_in(built_in, self.output))
            }
        }
    }
}

impl ShaderStage {
    const fn to_str(self) -> &'static str {
        match self {
            ShaderStage::Compute => "cs",
            ShaderStage::Fragment => "fs",
            ShaderStage::Vertex => "vs",
        }
    }
}

/// Shorthand result used internally by the backend
type BackendResult<T = ()> = Result<T, Error>;

/// A GLSL compilation error.
#[derive(Debug, Error)]
pub enum Error {
    /// A error occurred while writing to the output.
    #[error("Format error")]
    FmtError(#[from] FmtError),
    /// The specified [`Version`] doesn't have all required [`Features`].
    ///
    /// Contains the missing [`Features`].
    #[error("The selected version doesn't support {0:?}")]
    MissingFeatures(Features),
    /// [`AddressSpace::PushConstant`](crate::AddressSpace::PushConstant) was used more than
    /// once in the entry point, which isn't supported.
    #[error("Multiple push constants aren't supported")]
    MultiplePushConstants,
    /// The specified [`Version`] isn't supported.
    #[error("The specified version isn't supported")]
    VersionNotSupported,
    /// The entry point couldn't be found.
    #[error("The requested entry point couldn't be found")]
    EntryPointNotFound,
    /// A call was made to an unsupported external.
    #[error("A call was made to an unsupported external: {0}")]
    UnsupportedExternal(String),
    /// A scalar with an unsupported width was requested.
    #[error("A scalar with an unsupported width was requested: {0:?} {1:?}")]
    UnsupportedScalar(crate::ScalarKind, crate::Bytes),
    /// A image was used with multiple samplers, which isn't supported.
    #[error("A image was used with multiple samplers")]
    ImageMultipleSamplers,
    #[error("{0}")]
    Custom(String),
}

/// Binary operation with a different logic on the GLSL side.
enum BinaryOperation {
    /// Vector comparison should use the function like `greaterThan()`, etc.
    VectorCompare,
    /// Vector component wise operation; used to polyfill unsupported ops like `|` and `&` for `bvecN`'s
    VectorComponentWise,
    /// GLSL `%` is SPIR-V `OpUMod/OpSMod` and `mod()` is `OpFMod`, but [`BinaryOperator::Modulo`](crate::BinaryOperator::Modulo) is `OpFRem`.
    Modulo,
    /// Any plain operation. No additional logic required.
    Other,
}

/// Writer responsible for all code generation.
pub struct Writer<'a, W> {
    // Inputs
    /// The module being written.
    module: &'a crate::Module,
    /// The module analysis.
    info: &'a valid::ModuleInfo,
    /// The output writer.
    out: W,
    /// User defined configuration to be used.
    options: &'a Options,

    // Internal State
    /// Features manager used to store all the needed features and write them.
    features: FeaturesManager,
    namer: proc::Namer,
    /// A map with all the names needed for writing the module
    /// (generated by a [`Namer`](crate::proc::Namer)).
    names: crate::FastHashMap<NameKey, String>,
    /// A map with the names of global variables needed for reflections.
    reflection_names_globals: crate::FastHashMap<Handle<crate::GlobalVariable>, String>,
    /// The selected entry point.
    entry_point: &'a crate::EntryPoint,
    /// The index of the selected entry point.
    entry_point_idx: proc::EntryPointIndex,
    /// A generator for unique block numbers.
    block_id: IdGenerator,
    /// Set of expressions that have associated temporary variables.
    named_expressions: crate::NamedExpressions,
    /// Set of expressions that need to be baked to avoid unnecessary repetition in output
    need_bake_expressions: back::NeedBakeExpressions,
}

impl<'a, W: Write> Writer<'a, W> {
    /// Creates a new [`Writer`] instance.
    ///
    /// # Errors
    /// - If the version specified is invalid or supported.
    /// - If the entry point couldn't be found in the module.
    /// - If the version specified doesn't support some used features.
    pub fn new(
        out: W,
        module: &'a crate::Module,
        info: &'a valid::ModuleInfo,
        options: &'a Options,
        pipeline_options: &'a PipelineOptions,
    ) -> Result<Self, Error> {
        // Check if the requested version is supported
        if !options.version.is_supported() {
            log::error!("Version {}", options.version);
            return Err(Error::VersionNotSupported);
        }

        // Try to find the entry point and corresponding index
        let ep_idx = module
            .entry_points
            .iter()
            .position(|ep| {
                pipeline_options.shader_stage == ep.stage && pipeline_options.entry_point == ep.name
            })
            .ok_or(Error::EntryPointNotFound)?;

        // Generate a map with names required to write the module
        let mut names = crate::FastHashMap::default();
        let mut namer = proc::Namer::default();
        namer.reset(module, keywords::RESERVED_KEYWORDS, &["gl_"], &mut names);

        // Build the instance
        let mut this = Self {
            module,
            info,
            out,
            options,
            namer,
            features: FeaturesManager::new(),
            names,
            reflection_names_globals: crate::FastHashMap::default(),
            entry_point: &module.entry_points[ep_idx],
            entry_point_idx: ep_idx as u16,

            block_id: IdGenerator::default(),
            named_expressions: Default::default(),
            need_bake_expressions: Default::default(),
        };

        // Find all features required to print this module
        this.collect_required_features()?;

        Ok(this)
    }

    /// Writes the [`Module`](crate::Module) as glsl to the output
    ///
    /// # Notes
    /// If an error occurs while writing, the output might have been written partially
    ///
    /// # Panics
    /// Might panic if the module is invalid
    pub fn write(&mut self) -> Result<ReflectionInfo, Error> {
        // We use `writeln!(self.out)` throughout the write to add newlines
        // to make the output more readable

        let es = self.options.version.is_es();

        // Write the version (It must be the first thing or it isn't a valid glsl output)
        writeln!(self.out, "#version {}", self.options.version)?;
        // Write all the needed extensions
        //
        // This used to be the last thing being written as it allowed to search for features while
        // writing the module saving some loops but some older versions (420 or less) required the
        // extensions to appear before being used, even though extensions are part of the
        // preprocessor not the processor ¯\_(ツ)_/¯
        self.features.write(self.options.version, &mut self.out)?;

        // Write the additional extensions
        if self
            .options
            .writer_flags
            .contains(WriterFlags::TEXTURE_SHADOW_LOD)
        {
            // https://www.khronos.org/registry/OpenGL/extensions/EXT/EXT_texture_shadow_lod.txt
            writeln!(self.out, "#extension GL_EXT_texture_shadow_lod : require")?;
        }

        // glsl es requires a precision to be specified for floats and ints
        // TODO: Should this be user configurable?
        if es {
            writeln!(self.out)?;
            writeln!(self.out, "precision highp float;")?;
            writeln!(self.out, "precision highp int;")?;
            writeln!(self.out)?;
        }

        if self.entry_point.stage == ShaderStage::Compute {
            let workgroup_size = self.entry_point.workgroup_size;
            writeln!(
                self.out,
                "layout(local_size_x = {}, local_size_y = {}, local_size_z = {}) in;",
                workgroup_size[0], workgroup_size[1], workgroup_size[2]
            )?;
            writeln!(self.out)?;
        }

        // Enable early depth tests if needed
        if let Some(depth_test) = self.entry_point.early_depth_test {
            // If early depth test is supported for this version of GLSL
            if self.options.version.supports_early_depth_test() {
                writeln!(self.out, "layout(early_fragment_tests) in;")?;

                if let Some(conservative) = depth_test.conservative {
                    use crate::ConservativeDepth as Cd;

                    let depth = match conservative {
                        Cd::GreaterEqual => "greater",
                        Cd::LessEqual => "less",
                        Cd::Unchanged => "unchanged",
                    };
                    writeln!(self.out, "layout (depth_{}) out float gl_FragDepth;", depth)?;
                }
                writeln!(self.out)?;
            } else {
                log::warn!(
                    "Early depth testing is not supported for this version of GLSL: {}",
                    self.options.version
                );
            }
        }

        let ep_info = self.info.get_entry_point(self.entry_point_idx as usize);

        // Write struct types.
        //
        // This are always ordered because the IR is structured in a way that
        // you can't make a struct without adding all of its members first.
        for (handle, ty) in self.module.types.iter() {
            if let TypeInner::Struct { ref members, .. } = ty.inner {
                // Structures ending with runtime-sized arrays can only be
                // rendered as shader storage blocks in GLSL, not stand-alone
                // struct types.
                if !self.module.types[members.last().unwrap().ty]
                    .inner
                    .is_dynamically_sized(&self.module.types)
                {
                    let name = &self.names[&NameKey::Type(handle)];
                    write!(self.out, "struct {} ", name)?;
                    self.write_struct_body(handle, members)?;
                    writeln!(self.out, ";")?;
                }
            }
        }

        // Write the globals
        //
        // We filter all globals that aren't used by the selected entry point as they might be
        // interfere with each other (i.e. two globals with the same location but different with
        // different classes)
        for (handle, global) in self.module.global_variables.iter() {
            if ep_info[handle].is_empty() {
                continue;
            }

            match self.module.types[global.ty].inner {
                // We treat images separately because they might require
                // writing the storage format
                TypeInner::Image {
                    mut dim,
                    arrayed,
                    class,
                } => {
                    // Gather the storage format if needed
                    let storage_format_access = match self.module.types[global.ty].inner {
                        TypeInner::Image {
                            class: crate::ImageClass::Storage { format, access },
                            ..
                        } => Some((format, access)),
                        _ => None,
                    };

                    if dim == crate::ImageDimension::D1 && es {
                        dim = crate::ImageDimension::D2
                    }

                    // Gether the location if needed
                    let layout_binding = if self.options.version.supports_explicit_locations() {
                        let br = global.binding.as_ref().unwrap();
                        self.options.binding_map.get(br).cloned()
                    } else {
                        None
                    };

                    // Write all the layout qualifiers
                    if layout_binding.is_some() || storage_format_access.is_some() {
                        write!(self.out, "layout(")?;
                        if let Some(binding) = layout_binding {
                            write!(self.out, "binding = {}", binding)?;
                        }
                        if let Some((format, _)) = storage_format_access {
                            let format_str = glsl_storage_format(format);
                            let separator = match layout_binding {
                                Some(_) => ",",
                                None => "",
                            };
                            write!(self.out, "{}{}", separator, format_str)?;
                        }
                        write!(self.out, ") ")?;
                    }

                    if let Some((_, access)) = storage_format_access {
                        self.write_storage_access(access)?;
                    }

                    // All images in glsl are `uniform`
                    // The trailing space is important
                    write!(self.out, "uniform ")?;

                    // write the type
                    //
                    // This is way we need the leading space because `write_image_type` doesn't add
                    // any spaces at the beginning or end
                    self.write_image_type(dim, arrayed, class)?;

                    // Finally write the name and end the global with a `;`
                    // The leading space is important
                    let global_name = self.get_global_name(handle, global);
                    writeln!(self.out, " {};", global_name)?;
                    writeln!(self.out)?;

                    self.reflection_names_globals.insert(handle, global_name);
                }
                // glsl has no concept of samplers so we just ignore it
                TypeInner::Sampler { .. } => continue,
                // All other globals are written by `write_global`
                _ => {
                    if !ep_info[handle].is_empty() {
                        self.write_global(handle, global)?;
                        // Add a newline (only for readability)
                        writeln!(self.out)?;
                    }
                }
            }
        }

        for arg in self.entry_point.function.arguments.iter() {
            self.write_varying(arg.binding.as_ref(), arg.ty, false)?;
        }
        if let Some(ref result) = self.entry_point.function.result {
            self.write_varying(result.binding.as_ref(), result.ty, true)?;
        }
        writeln!(self.out)?;

        // Write all regular functions
        for (handle, function) in self.module.functions.iter() {
            // Check that the function doesn't use globals that aren't supported
            // by the current entry point
            if !ep_info.dominates_global_use(&self.info[handle]) {
                continue;
            }

            let fun_info = &self.info[handle];

            // Write the function
            self.write_function(back::FunctionType::Function(handle), function, fun_info)?;

            writeln!(self.out)?;
        }

        self.write_function(
            back::FunctionType::EntryPoint(self.entry_point_idx),
            &self.entry_point.function,
            ep_info,
        )?;

        // Add newline at the end of file
        writeln!(self.out)?;

        // Collect all reflection info and return it to the user
        self.collect_reflection_info()
    }

    fn write_array_size(
        &mut self,
        base: Handle<crate::Type>,
        size: crate::ArraySize,
    ) -> BackendResult {
        write!(self.out, "[")?;

        // Write the array size
        // Writes nothing if `ArraySize::Dynamic`
        // Panics if `ArraySize::Constant` has a constant that isn't an sint or uint
        match size {
            crate::ArraySize::Constant(const_handle) => {
                match self.module.constants[const_handle].inner {
                    crate::ConstantInner::Scalar {
                        width: _,
                        value: crate::ScalarValue::Uint(size),
                    } => write!(self.out, "{}", size)?,
                    crate::ConstantInner::Scalar {
                        width: _,
                        value: crate::ScalarValue::Sint(size),
                    } => write!(self.out, "{}", size)?,
                    _ => unreachable!(),
                }
            }
            crate::ArraySize::Dynamic => (),
        }

        write!(self.out, "]")?;

        if let TypeInner::Array {
            base: next_base,
            size: next_size,
            ..
        } = self.module.types[base].inner
        {
            self.write_array_size(next_base, next_size)?;
        }

        Ok(())
    }

    /// Helper method used to write value types
    ///
    /// # Notes
    /// Adds no trailing or leading whitespace
    ///
    /// # Panics
    /// - If type is either a image, a sampler, a pointer, or a struct
    /// - If it's an Array with a [`ArraySize::Constant`](crate::ArraySize::Constant) with a
    /// constant that isn't a [`Scalar`](crate::ConstantInner::Scalar) or if the
    /// scalar value isn't an [`Sint`](crate::ScalarValue::Sint) or [`Uint`](crate::ScalarValue::Uint)
    fn write_value_type(&mut self, inner: &TypeInner) -> BackendResult {
        match *inner {
            // Scalars are simple we just get the full name from `glsl_scalar`
            TypeInner::Scalar { kind, width }
            | TypeInner::Atomic { kind, width }
            | TypeInner::ValuePointer {
                size: None,
                kind,
                width,
                space: _,
            } => write!(self.out, "{}", glsl_scalar(kind, width)?.full)?,
            // Vectors are just `gvecN` where `g` is the scalar prefix and `N` is the vector size
            TypeInner::Vector { size, kind, width }
            | TypeInner::ValuePointer {
                size: Some(size),
                kind,
                width,
                space: _,
            } => write!(
                self.out,
                "{}vec{}",
                glsl_scalar(kind, width)?.prefix,
                size as u8
            )?,
            // Matrices are written with `gmatMxN` where `g` is the scalar prefix (only floats and
            // doubles are allowed), `M` is the columns count and `N` is the rows count
            //
            // glsl supports a matrix shorthand `gmatN` where `N` = `M` but it doesn't justify the
            // extra branch to write matrices this way
            TypeInner::Matrix {
                columns,
                rows,
                width,
            } => write!(
                self.out,
                "{}mat{}x{}",
                glsl_scalar(crate::ScalarKind::Float, width)?.prefix,
                columns as u8,
                rows as u8
            )?,
            // GLSL arrays are written as `type name[size]`
            // Current code is written arrays only as `[size]`
            // Base `type` and `name` should be written outside
            TypeInner::Array { base, size, .. } => self.write_array_size(base, size)?,
            // Panic if either Image, Sampler, Pointer, or a Struct is being written
            //
            // Write all variants instead of `_` so that if new variants are added a
            // no exhaustiveness error is thrown
            TypeInner::Pointer { .. }
            | TypeInner::Struct { .. }
            | TypeInner::Image { .. }
            | TypeInner::Sampler { .. }
            | TypeInner::BindingArray { .. } => {
                return Err(Error::Custom(format!("Unable to write type {:?}", inner)))
            }
        }

        Ok(())
    }

    /// Helper method used to write non image/sampler types
    ///
    /// # Notes
    /// Adds no trailing or leading whitespace
    ///
    /// # Panics
    /// - If type is either a image or sampler
    /// - If it's an Array with a [`ArraySize::Constant`](crate::ArraySize::Constant) with a
    /// constant that isn't a [`Scalar`](crate::ConstantInner::Scalar) or if the
    /// scalar value isn't an [`Sint`](crate::ScalarValue::Sint) or [`Uint`](crate::ScalarValue::Uint)
    fn write_type(&mut self, ty: Handle<crate::Type>) -> BackendResult {
        match self.module.types[ty].inner {
            // glsl has no pointer types so just write types as normal and loads are skipped
            TypeInner::Pointer { base, .. } => self.write_type(base),
            // glsl structs are written as just the struct name
            TypeInner::Struct { .. } => {
                // Get the struct name
                let name = &self.names[&NameKey::Type(ty)];
                write!(self.out, "{}", name)?;
                Ok(())
            }
            // glsl array has the size separated from the base type
            TypeInner::Array { base, .. } => self.write_type(base),
            ref other => self.write_value_type(other),
        }
    }

    /// Helper method to write a image type
    ///
    /// # Notes
    /// Adds no leading or trailing whitespace
    fn write_image_type(
        &mut self,
        dim: crate::ImageDimension,
        arrayed: bool,
        class: crate::ImageClass,
    ) -> BackendResult {
        // glsl images consist of four parts the scalar prefix, the image "type", the dimensions
        // and modifiers
        //
        // There exists two image types
        // - sampler - for sampled images
        // - image - for storage images
        //
        // There are three possible modifiers that can be used together and must be written in
        // this order to be valid
        // - MS - used if it's a multisampled image
        // - Array - used if it's an image array
        // - Shadow - used if it's a depth image
        use crate::ImageClass as Ic;

        let (base, kind, ms, comparison) = match class {
            Ic::Sampled { kind, multi: true } => ("sampler", kind, "MS", ""),
            Ic::Sampled { kind, multi: false } => ("sampler", kind, "", ""),
            Ic::Depth { multi: true } => ("sampler", crate::ScalarKind::Float, "MS", ""),
            Ic::Depth { multi: false } => ("sampler", crate::ScalarKind::Float, "", "Shadow"),
            Ic::Storage { format, .. } => ("image", format.into(), "", ""),
        };

        write!(
            self.out,
            "highp {}{}{}{}{}{}",
            glsl_scalar(kind, 4)?.prefix,
            base,
            glsl_dimension(dim),
            ms,
            if arrayed { "Array" } else { "" },
            comparison
        )?;

        Ok(())
    }

    /// Helper method used to write non images/sampler globals
    ///
    /// # Notes
    /// Adds a newline
    ///
    /// # Panics
    /// If the global has type sampler
    fn write_global(
        &mut self,
        handle: Handle<crate::GlobalVariable>,
        global: &crate::GlobalVariable,
    ) -> BackendResult {
        if self.options.version.supports_explicit_locations() {
            if let Some(ref br) = global.binding {
                match self.options.binding_map.get(br) {
                    Some(binding) => {
                        let layout = match global.space {
                            crate::AddressSpace::Storage { .. } => {
                                if self.options.version.supports_std430_layout() {
                                    "std430, "
                                } else {
                                    "std140, "
                                }
                            }
                            crate::AddressSpace::Uniform => "std140, ",
                            _ => "",
                        };
                        write!(self.out, "layout({}binding = {}) ", layout, binding)?
                    }
                    None => {
                        log::debug!("unassigned binding for {:?}", global.name);
                        if let crate::AddressSpace::Storage { .. } = global.space {
                            if self.options.version.supports_std430_layout() {
                                write!(self.out, "layout(std430) ")?
                            }
                        }
                    }
                }
            }
        }

        if let crate::AddressSpace::Storage { access } = global.space {
            self.write_storage_access(access)?;
        }

        if let Some(storage_qualifier) = glsl_storage_qualifier(global.space) {
            write!(self.out, "{} ", storage_qualifier)?;
        }

        match global.space {
            crate::AddressSpace::Private => {
                self.write_simple_global(handle, global)?;
            }
            crate::AddressSpace::WorkGroup => {
                self.write_simple_global(handle, global)?;
            }
            crate::AddressSpace::PushConstant => {
                self.write_simple_global(handle, global)?;
            }
            crate::AddressSpace::Uniform => {
                self.write_interface_block(handle, global)?;
            }
            crate::AddressSpace::Storage { .. } => {
                self.write_interface_block(handle, global)?;
            }
            // A global variable in the `Function` address space is a
            // contradiction in terms.
            crate::AddressSpace::Function => unreachable!(),
            // Textures and samplers are handled directly in `Writer::write`.
            crate::AddressSpace::Handle => unreachable!(),
        }

        Ok(())
    }

    fn write_simple_global(
        &mut self,
        handle: Handle<crate::GlobalVariable>,
        global: &crate::GlobalVariable,
    ) -> BackendResult {
        self.write_type(global.ty)?;
        write!(self.out, " ")?;
        self.write_global_name(handle, global)?;

        if let TypeInner::Array { base, size, .. } = self.module.types[global.ty].inner {
            self.write_array_size(base, size)?;
        }

        if global.space.initializable() && is_value_init_supported(self.module, global.ty) {
            write!(self.out, " = ")?;
            if let Some(init) = global.init {
                self.write_constant(init)?;
            } else {
                self.write_zero_init_value(global.ty)?;
            }
        }

        writeln!(self.out, ";")?;

        if let crate::AddressSpace::PushConstant = global.space {
            let global_name = self.get_global_name(handle, global);
            self.reflection_names_globals.insert(handle, global_name);
        }

        Ok(())
    }

    /// Write an interface block for a single Naga global.
    ///
    /// Write `block_name { members }`. Since `block_name` must be unique
    /// between blocks and structs, we add `_block_ID` where `ID` is a
    /// `IdGenerator` generated number. Write `members` in the same way we write
    /// a struct's members.
    fn write_interface_block(
        &mut self,
        handle: Handle<crate::GlobalVariable>,
        global: &crate::GlobalVariable,
    ) -> BackendResult {
        // Write the block name, it's just the struct name appended with `_block_ID`
        let ty_name = &self.names[&NameKey::Type(global.ty)];
        let block_name = format!(
            "{}_block_{}{:?}",
            ty_name,
            self.block_id.generate(),
            self.entry_point.stage,
        );
        write!(self.out, "{} ", block_name)?;
        self.reflection_names_globals.insert(handle, block_name);

        match self.module.types[global.ty].inner {
            crate::TypeInner::Struct { ref members, .. }
                if self.module.types[members.last().unwrap().ty]
                    .inner
                    .is_dynamically_sized(&self.module.types) =>
            {
                // Structs with dynamically sized arrays must have their
                // members lifted up as members of the interface block. GLSL
                // can't write such struct types anyway.
                self.write_struct_body(global.ty, members)?;
                write!(self.out, " ")?;
                self.write_global_name(handle, global)?;
            }
            _ => {
                // A global of any other type is written as the sole member
                // of the interface block. Since the interface block is
                // anonymous, this becomes visible in the global scope.
                write!(self.out, "{{ ")?;
                self.write_type(global.ty)?;
                write!(self.out, " ")?;
                self.write_global_name(handle, global)?;
                if let TypeInner::Array { base, size, .. } = self.module.types[global.ty].inner {
                    self.write_array_size(base, size)?;
                }
                write!(self.out, "; }}")?;
            }
        }

        writeln!(self.out, ";")?;

        Ok(())
    }

    /// Helper method used to find which expressions of a given function require baking
    ///
    /// # Notes
    /// Clears `need_bake_expressions` set before adding to it
    fn update_expressions_to_bake(&mut self, func: &crate::Function, info: &valid::FunctionInfo) {
        use crate::Expression;
        self.need_bake_expressions.clear();
        for expr in func.expressions.iter() {
            let expr_info = &info[expr.0];
            let min_ref_count = func.expressions[expr.0].bake_ref_count();
            if min_ref_count <= expr_info.ref_count {
                self.need_bake_expressions.insert(expr.0);
            }
            // if the expression is a Dot product with integer arguments,
            // then the args needs baking as well
            if let (
                fun_handle,
                &Expression::Math {
                    fun: crate::MathFunction::Dot,
                    arg,
                    arg1,
                    ..
                },
            ) = expr
            {
                let inner = info[fun_handle].ty.inner_with(&self.module.types);
                if let TypeInner::Scalar { kind, .. } = *inner {
                    match kind {
                        crate::ScalarKind::Sint | crate::ScalarKind::Uint => {
                            self.need_bake_expressions.insert(arg);
                            self.need_bake_expressions.insert(arg1.unwrap());
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    /// Helper method used to get a name for a global
    ///
    /// Globals have different naming schemes depending on their binding:
    /// - Globals without bindings use the name from the [`Namer`](crate::proc::Namer)
    /// - Globals with resource binding are named `_group_X_binding_Y` where `X`
    ///   is the group and `Y` is the binding
    fn get_global_name(
        &self,
        handle: Handle<crate::GlobalVariable>,
        global: &crate::GlobalVariable,
    ) -> String {
        match global.binding {
            Some(ref br) => {
                format!(
                    "_group_{}_binding_{}_{}",
                    br.group,
                    br.binding,
                    self.entry_point.stage.to_str()
                )
            }
            None => self.names[&NameKey::GlobalVariable(handle)].clone(),
        }
    }

    /// Helper method used to write a name for a global without additional heap allocation
    fn write_global_name(
        &mut self,
        handle: Handle<crate::GlobalVariable>,
        global: &crate::GlobalVariable,
    ) -> BackendResult {
        match global.binding {
            Some(ref br) => write!(
                self.out,
                "_group_{}_binding_{}_{}",
                br.group,
                br.binding,
                self.entry_point.stage.to_str()
            )?,
            None => write!(
                self.out,
                "{}",
                &self.names[&NameKey::GlobalVariable(handle)]
            )?,
        }

        Ok(())
    }

    /// Write a GLSL global that will carry a Naga entry point's argument or return value.
    ///
    /// A Naga entry point's arguments and return value are rendered in GLSL as
    /// variables at global scope with the `in` and `out` storage qualifiers.
    /// The code we generate for `main` loads from all the `in` globals into
    /// appropriately named locals. Before it returns, `main` assigns the
    /// components of its return value into all the `out` globals.
    ///
    /// This function writes a declaration for one such GLSL global,
    /// representing a value passed into or returned from [`self.entry_point`]
    /// that has a [`Location`] binding. The global's name is generated based on
    /// the location index and the shader stages being connected; see
    /// [`VaryingName`]. This means we don't need to know the names of
    /// arguments, just their types and bindings.
    ///
    /// Emit nothing for entry point arguments or return values with [`BuiltIn`]
    /// bindings; `main` will read from or assign to the appropriate GLSL
    /// special variable; these are pre-declared. As an exception, we do declare
    /// `gl_Position` or `gl_FragCoord` with the `invariant` qualifier if
    /// needed.
    ///
    /// Use `output` together with [`self.entry_point.stage`] to determine which
    /// shader stages are being connected, and choose the `in` or `out` storage
    /// qualifier.
    ///
    /// [`self.entry_point`]: Writer::entry_point
    /// [`self.entry_point.stage`]: crate::EntryPoint::stage
    /// [`Location`]: crate::Binding::Location
    /// [`BuiltIn`]: crate::Binding::BuiltIn
    fn write_varying(
        &mut self,
        binding: Option<&crate::Binding>,
        ty: Handle<crate::Type>,
        output: bool,
    ) -> Result<(), Error> {
        // For a struct, emit a separate global for each member with a binding.
        if let crate::TypeInner::Struct { ref members, .. } = self.module.types[ty].inner {
            for member in members {
                self.write_varying(member.binding.as_ref(), member.ty, output)?;
            }
            return Ok(());
        }

        let binding = match binding {
            None => return Ok(()),
            Some(binding) => binding,
        };

        let (location, interpolation, sampling) = match *binding {
            crate::Binding::Location {
                location,
                interpolation,
                sampling,
            } => (location, interpolation, sampling),
            crate::Binding::BuiltIn(built_in) => {
                if let crate::BuiltIn::Position { invariant: true } = built_in {
                    writeln!(self.out, "invariant {};", glsl_built_in(built_in, output))?;
                }
                return Ok(());
            }
        };

        // Write the interpolation modifier if needed
        //
        // We ignore all interpolation and auxiliary modifiers that aren't used in fragment
        // shaders' input globals or vertex shaders' output globals.
        let emit_interpolation_and_auxiliary = match self.entry_point.stage {
            ShaderStage::Vertex => output,
            ShaderStage::Fragment => !output,
            _ => false,
        };

        // Write the I/O locations, if allowed
        if self.options.version.supports_explicit_locations() || !emit_interpolation_and_auxiliary {
            write!(self.out, "layout(location = {}) ", location)?;
        }

        // Write the interpolation qualifier.
        if let Some(interp) = interpolation {
            if emit_interpolation_and_auxiliary {
                write!(self.out, "{} ", glsl_interpolation(interp))?;
            }
        }

        // Write the sampling auxiliary qualifier.
        //
        // Before GLSL 4.2, the `centroid` and `sample` qualifiers were required to appear
        // immediately before the `in` / `out` qualifier, so we'll just follow that rule
        // here, regardless of the version.
        if let Some(sampling) = sampling {
            if emit_interpolation_and_auxiliary {
                if let Some(qualifier) = glsl_sampling(sampling) {
                    write!(self.out, "{} ", qualifier)?;
                }
            }
        }

        // Write the input/output qualifier.
        write!(self.out, "{} ", if output { "out" } else { "in" })?;

        // Write the type
        // `write_type` adds no leading or trailing spaces
        self.write_type(ty)?;

        // Finally write the global name and end the global with a `;` and a newline
        // Leading space is important
        let vname = VaryingName {
            binding: &crate::Binding::Location {
                location,
                interpolation: None,
                sampling: None,
            },
            stage: self.entry_point.stage,
            output,
        };
        writeln!(self.out, " {};", vname)?;

        Ok(())
    }

    /// Helper method used to write functions (both entry points and regular functions)
    ///
    /// # Notes
    /// Adds a newline
    fn write_function(
        &mut self,
        ty: back::FunctionType,
        func: &crate::Function,
        info: &valid::FunctionInfo,
    ) -> BackendResult {
        // Create a function context for the function being written
        let ctx = back::FunctionCtx {
            ty,
            info,
            expressions: &func.expressions,
            named_expressions: &func.named_expressions,
        };

        self.named_expressions.clear();
        self.update_expressions_to_bake(func, info);

        // Write the function header
        //
        // glsl headers are the same as in c:
        // `ret_type name(args)`
        // `ret_type` is the return type
        // `name` is the function name
        // `args` is a comma separated list of `type name`
        //  | - `type` is the argument type
        //  | - `name` is the argument name

        // Start by writing the return type if any otherwise write void
        // This is the only place where `void` is a valid type
        // (though it's more a keyword than a type)
        if let back::FunctionType::EntryPoint(_) = ctx.ty {
            write!(self.out, "void")?;
        } else if let Some(ref result) = func.result {
            self.write_type(result.ty)?;
        } else {
            write!(self.out, "void")?;
        }

        // Write the function name and open parentheses for the argument list
        let function_name = match ctx.ty {
            back::FunctionType::Function(handle) => &self.names[&NameKey::Function(handle)],
            back::FunctionType::EntryPoint(_) => "main",
        };
        write!(self.out, " {}(", function_name)?;

        // Write the comma separated argument list
        //
        // We need access to `Self` here so we use the reference passed to the closure as an
        // argument instead of capturing as that would cause a borrow checker error
        let arguments = match ctx.ty {
            back::FunctionType::EntryPoint(_) => &[][..],
            back::FunctionType::Function(_) => &func.arguments,
        };
        let arguments: Vec<_> = arguments
            .iter()
            .enumerate()
            .filter(|&(_, arg)| match self.module.types[arg.ty].inner {
                TypeInner::Sampler { .. } => false,
                _ => true,
            })
            .collect();
        self.write_slice(&arguments, |this, _, &(i, arg)| {
            // Write the argument type
            match this.module.types[arg.ty].inner {
                // We treat images separately because they might require
                // writing the storage format
                TypeInner::Image {
                    dim,
                    arrayed,
                    class,
                } => {
                    // Write the storage format if needed
                    if let TypeInner::Image {
                        class: crate::ImageClass::Storage { format, .. },
                        ..
                    } = this.module.types[arg.ty].inner
                    {
                        write!(this.out, "layout({}) ", glsl_storage_format(format))?;
                    }

                    // write the type
                    //
                    // This is way we need the leading space because `write_image_type` doesn't add
                    // any spaces at the beginning or end
                    this.write_image_type(dim, arrayed, class)?;
                }
                TypeInner::Pointer { base, .. } => {
                    // write parameter qualifiers
                    write!(this.out, "inout ")?;
                    this.write_type(base)?;
                }
                // All other types are written by `write_type`
                _ => {
                    this.write_type(arg.ty)?;
                }
            }

            // Write the argument name
            // The leading space is important
            write!(this.out, " {}", &this.names[&ctx.argument_key(i as u32)])?;

            // Write array size
            if let TypeInner::Array { base, size, .. } = this.module.types[arg.ty].inner {
                this.write_array_size(base, size)?;
            }

            Ok(())
        })?;

        // Close the parentheses and open braces to start the function body
        writeln!(self.out, ") {{")?;

        // Compose the function arguments from globals, in case of an entry point.
        if let back::FunctionType::EntryPoint(ep_index) = ctx.ty {
            let stage = self.module.entry_points[ep_index as usize].stage;
            for (index, arg) in func.arguments.iter().enumerate() {
                write!(self.out, "{}", back::INDENT)?;
                self.write_type(arg.ty)?;
                let name = &self.names[&NameKey::EntryPointArgument(ep_index, index as u32)];
                write!(self.out, " {}", name)?;
                write!(self.out, " = ")?;
                match self.module.types[arg.ty].inner {
                    crate::TypeInner::Struct { ref members, .. } => {
                        self.write_type(arg.ty)?;
                        write!(self.out, "(")?;
                        for (index, member) in members.iter().enumerate() {
                            let varying_name = VaryingName {
                                binding: member.binding.as_ref().unwrap(),
                                stage,
                                output: false,
                            };
                            if index != 0 {
                                write!(self.out, ", ")?;
                            }
                            write!(self.out, "{}", varying_name)?;
                        }
                        writeln!(self.out, ");")?;
                    }
                    _ => {
                        let varying_name = VaryingName {
                            binding: arg.binding.as_ref().unwrap(),
                            stage,
                            output: false,
                        };
                        writeln!(self.out, "{};", varying_name)?;
                    }
                }
            }
        }

        // Write all function locals
        // Locals are `type name (= init)?;` where the init part (including the =) are optional
        //
        // Always adds a newline
        for (handle, local) in func.local_variables.iter() {
            // Write indentation (only for readability) and the type
            // `write_type` adds no trailing space
            write!(self.out, "{}", back::INDENT)?;
            self.write_type(local.ty)?;

            // Write the local name
            // The leading space is important
            write!(self.out, " {}", self.names[&ctx.name_key(handle)])?;
            // Write size for array type
            if let TypeInner::Array { base, size, .. } = self.module.types[local.ty].inner {
                self.write_array_size(base, size)?;
            }
            // Write the local initializer if needed
            if let Some(init) = local.init {
                // Put the equal signal only if there's a initializer
                // The leading and trailing spaces aren't needed but help with readability
                write!(self.out, " = ")?;

                // Write the constant
                // `write_constant` adds no trailing or leading space/newline
                self.write_constant(init)?;
            } else if is_value_init_supported(self.module, local.ty) {
                write!(self.out, " = ")?;
                self.write_zero_init_value(local.ty)?;
            }

            // Finish the local with `;` and add a newline (only for readability)
            writeln!(self.out, ";")?
        }

        // Write the function body (statement list)
        for sta in func.body.iter() {
            // Write a statement, the indentation should always be 1 when writing the function body
            // `write_stmt` adds a newline
            self.write_stmt(sta, &ctx, back::Level(1))?;
        }

        // Close braces and add a newline
        writeln!(self.out, "}}")?;

        Ok(())
    }

    /// Helper method that writes a list of comma separated `T` with a writer function `F`
    ///
    /// The writer function `F` receives a mutable reference to `self` that if needed won't cause
    /// borrow checker issues (using for example a closure with `self` will cause issues), the
    /// second argument is the 0 based index of the element on the list, and the last element is
    /// a reference to the element `T` being written
    ///
    /// # Notes
    /// - Adds no newlines or leading/trailing whitespace
    /// - The last element won't have a trailing `,`
    fn write_slice<T, F: FnMut(&mut Self, u32, &T) -> BackendResult>(
        &mut self,
        data: &[T],
        mut f: F,
    ) -> BackendResult {
        // Loop trough `data` invoking `f` for each element
        for (i, item) in data.iter().enumerate() {
            f(self, i as u32, item)?;

            // Only write a comma if isn't the last element
            if i != data.len().saturating_sub(1) {
                // The leading space is for readability only
                write!(self.out, ", ")?;
            }
        }

        Ok(())
    }

    /// Helper method used to write constants
    ///
    /// # Notes
    /// Adds no newlines or leading/trailing whitespace
    fn write_constant(&mut self, handle: Handle<crate::Constant>) -> BackendResult {
        use crate::ScalarValue as Sv;

        match self.module.constants[handle].inner {
            crate::ConstantInner::Scalar {
                width: _,
                ref value,
            } => match *value {
                // Signed integers don't need anything special
                Sv::Sint(int) => write!(self.out, "{}", int)?,
                // Unsigned integers need a `u` at the end
                //
                // While `core` doesn't necessarily need it, it's allowed and since `es` needs it we
                // always write it as the extra branch wouldn't have any benefit in readability
                Sv::Uint(int) => write!(self.out, "{}u", int)?,
                // Floats are written using `Debug` instead of `Display` because it always appends the
                // decimal part even it's zero which is needed for a valid glsl float constant
                Sv::Float(float) => write!(self.out, "{:?}", float)?,
                // Booleans are either `true` or `false` so nothing special needs to be done
                Sv::Bool(boolean) => write!(self.out, "{}", boolean)?,
            },
            // Composite constant are created using the same syntax as compose
            // `type(components)` where `components` is a comma separated list of constants
            crate::ConstantInner::Composite { ty, ref components } => {
                self.write_type(ty)?;
                if let TypeInner::Array { base, size, .. } = self.module.types[ty].inner {
                    self.write_array_size(base, size)?;
                }
                write!(self.out, "(")?;

                // Write the comma separated constants
                self.write_slice(components, |this, _, arg| this.write_constant(*arg))?;

                write!(self.out, ")")?
            }
        }

        Ok(())
    }

    /// Helper method used to output a dot product as an arithmetic expression
    ///
    fn write_dot_product(
        &mut self,
        arg: Handle<crate::Expression>,
        arg1: Handle<crate::Expression>,
        size: usize,
    ) -> BackendResult {
        write!(self.out, "(")?;

        let arg0_name = &self.named_expressions[&arg];
        let arg1_name = &self.named_expressions[&arg1];

        // This will print an extra '+' at the beginning but that is fine in glsl
        for index in 0..size {
            let component = back::COMPONENTS[index];
            write!(
                self.out,
                " + {}.{} * {}.{}",
                arg0_name, component, arg1_name, component
            )?;
        }

        write!(self.out, ")")?;
        Ok(())
    }

    /// Helper method used to write structs
    ///
    /// # Notes
    /// Ends in a newline
    fn write_struct_body(
        &mut self,
        handle: Handle<crate::Type>,
        members: &[crate::StructMember],
    ) -> BackendResult {
        // glsl structs are written as in C
        // `struct name() { members };`
        //  | `struct` is a keyword
        //  | `name` is the struct name
        //  | `members` is a semicolon separated list of `type name`
        //      | `type` is the member type
        //      | `name` is the member name
        writeln!(self.out, "{{")?;

        for (idx, member) in members.iter().enumerate() {
            // The indentation is only for readability
            write!(self.out, "{}", back::INDENT)?;

            match self.module.types[member.ty].inner {
                TypeInner::Array {
                    base,
                    size,
                    stride: _,
                } => {
                    self.write_type(base)?;
                    write!(
                        self.out,
                        " {}",
                        &self.names[&NameKey::StructMember(handle, idx as u32)]
                    )?;
                    // Write [size]
                    self.write_array_size(base, size)?;
                    // Newline is important
                    writeln!(self.out, ";")?;
                }
                _ => {
                    // Write the member type
                    // Adds no trailing space
                    self.write_type(member.ty)?;

                    // Write the member name and put a semicolon
                    // The leading space is important
                    // All members must have a semicolon even the last one
                    writeln!(
                        self.out,
                        " {};",
                        &self.names[&NameKey::StructMember(handle, idx as u32)]
                    )?;
                }
            }
        }

        write!(self.out, "}}")?;
        Ok(())
    }

    /// Helper method used to write statements
    ///
    /// # Notes
    /// Always adds a newline
    fn write_stmt(
        &mut self,
        sta: &crate::Statement,
        ctx: &back::FunctionCtx,
        level: back::Level,
    ) -> BackendResult {
        use crate::Statement;

        match *sta {
            // This is where we can generate intermediate constants for some expression types.
            Statement::Emit(ref range) => {
                for handle in range.clone() {
                    let info = &ctx.info[handle];
                    let ptr_class = info.ty.inner_with(&self.module.types).pointer_space();
                    let expr_name = if ptr_class.is_some() {
                        // GLSL can't save a pointer-valued expression in a variable,
                        // but we shouldn't ever need to: they should never be named expressions,
                        // and none of the expression types flagged by bake_ref_count can be pointer-valued.
                        None
                    } else if let Some(name) = ctx.named_expressions.get(&handle) {
                        // Front end provides names for all variables at the start of writing.
                        // But we write them to step by step. We need to recache them
                        // Otherwise, we could accidentally write variable name instead of full expression.
                        // Also, we use sanitized names! It defense backend from generating variable with name from reserved keywords.
                        Some(self.namer.call(name))
                    } else if self.need_bake_expressions.contains(&handle) {
                        Some(format!("{}{}", back::BAKE_PREFIX, handle.index()))
                    } else if info.ref_count == 0 {
                        Some(self.namer.call(""))
                    } else {
                        None
                    };

                    if let Some(name) = expr_name {
                        write!(self.out, "{}", level)?;
                        self.write_named_expr(handle, name, ctx)?;
                    }
                }
            }
            // Blocks are simple we just need to write the block statements between braces
            // We could also just print the statements but this is more readable and maps more
            // closely to the IR
            Statement::Block(ref block) => {
                write!(self.out, "{}", level)?;
                writeln!(self.out, "{{")?;
                for sta in block.iter() {
                    // Increase the indentation to help with readability
                    self.write_stmt(sta, ctx, level.next())?
                }
                writeln!(self.out, "{}}}", level)?
            }
            // Ifs are written as in C:
            // ```
            // if(condition) {
            //  accept
            // } else {
            //  reject
            // }
            // ```
            Statement::If {
                condition,
                ref accept,
                ref reject,
            } => {
                write!(self.out, "{}", level)?;
                write!(self.out, "if (")?;
                self.write_expr(condition, ctx)?;
                writeln!(self.out, ") {{")?;

                for sta in accept {
                    // Increase indentation to help with readability
                    self.write_stmt(sta, ctx, level.next())?;
                }

                // If there are no statements in the reject block we skip writing it
                // This is only for readability
                if !reject.is_empty() {
                    writeln!(self.out, "{}}} else {{", level)?;

                    for sta in reject {
                        // Increase indentation to help with readability
                        self.write_stmt(sta, ctx, level.next())?;
                    }
                }

                writeln!(self.out, "{}}}", level)?
            }
            // Switch are written as in C:
            // ```
            // switch (selector) {
            //      // Fallthrough
            //      case label:
            //          block
            //      // Non fallthrough
            //      case label:
            //          block
            //          break;
            //      default:
            //          block
            //  }
            //  ```
            //  Where the `default` case happens isn't important but we put it last
            //  so that we don't need to print a `break` for it
            Statement::Switch {
                selector,
                ref cases,
            } => {
                // Start the switch
                write!(self.out, "{}", level)?;
                write!(self.out, "switch(")?;
                self.write_expr(selector, ctx)?;
                writeln!(self.out, ") {{")?;
                let type_postfix = match *ctx.info[selector].ty.inner_with(&self.module.types) {
                    crate::TypeInner::Scalar {
                        kind: crate::ScalarKind::Uint,
                        ..
                    } => "u",
                    _ => "",
                };

                // Write all cases
                let l2 = level.next();
                for case in cases {
                    match case.value {
                        crate::SwitchValue::Integer(value) => {
                            writeln!(self.out, "{}case {}{}:", l2, value, type_postfix)?
                        }
                        crate::SwitchValue::Default => writeln!(self.out, "{}default:", l2)?,
                    }

                    for sta in case.body.iter() {
                        self.write_stmt(sta, ctx, l2.next())?;
                    }

                    // Write fallthrough comment if the case is fallthrough,
                    // otherwise write a break, if the case is not already
                    // broken out of at the end of its body.
                    if case.fall_through {
                        writeln!(self.out, "{}/* fallthrough */", l2.next())?;
                    } else if case.body.last().map_or(true, |s| !s.is_terminator()) {
                        writeln!(self.out, "{}break;", l2.next())?;
                    }
                }

                writeln!(self.out, "{}}}", level)?
            }
            // Loops in naga IR are based on wgsl loops, glsl can emulate the behaviour by using a
            // while true loop and appending the continuing block to the body resulting on:
            // ```
            // bool loop_init = true;
            // while(true) {
            //  if (!loop_init) { <continuing> }
            //  loop_init = false;
            //  <body>
            // }
            // ```
            Statement::Loop {
                ref body,
                ref continuing,
            } => {
                if !continuing.is_empty() {
                    let gate_name = self.namer.call("loop_init");
                    writeln!(self.out, "{}bool {} = true;", level, gate_name)?;
                    writeln!(self.out, "{}while(true) {{", level)?;
                    writeln!(self.out, "{}if (!{}) {{", level.next(), gate_name)?;
                    for sta in continuing {
                        self.write_stmt(sta, ctx, level.next())?;
                    }
                    writeln!(self.out, "{}}}", level.next())?;
                    writeln!(self.out, "{}{} = false;", level.next(), gate_name)?;
                } else {
                    writeln!(self.out, "{}while(true) {{", level)?;
                }
                for sta in body {
                    self.write_stmt(sta, ctx, level.next())?;
                }
                writeln!(self.out, "{}}}", level)?
            }
            // Break, continue and return as written as in C
            // `break;`
            Statement::Break => {
                write!(self.out, "{}", level)?;
                writeln!(self.out, "break;")?
            }
            // `continue;`
            Statement::Continue => {
                write!(self.out, "{}", level)?;
                writeln!(self.out, "continue;")?
            }
            // `return expr;`, `expr` is optional
            Statement::Return { value } => {
                write!(self.out, "{}", level)?;
                match ctx.ty {
                    back::FunctionType::Function(_) => {
                        write!(self.out, "return")?;
                        // Write the expression to be returned if needed
                        if let Some(expr) = value {
                            write!(self.out, " ")?;
                            self.write_expr(expr, ctx)?;
                        }
                        writeln!(self.out, ";")?;
                    }
                    back::FunctionType::EntryPoint(ep_index) => {
                        let ep = &self.module.entry_points[ep_index as usize];
                        if let Some(ref result) = ep.function.result {
                            let value = value.unwrap();
                            match self.module.types[result.ty].inner {
                                crate::TypeInner::Struct { ref members, .. } => {
                                    let temp_struct_name = match ctx.expressions[value] {
                                        crate::Expression::Compose { .. } => {
                                            let return_struct = "_tmp_return";
                                            write!(
                                                self.out,
                                                "{} {} = ",
                                                &self.names[&NameKey::Type(result.ty)],
                                                return_struct
                                            )?;
                                            self.write_expr(value, ctx)?;
                                            writeln!(self.out, ";")?;
                                            write!(self.out, "{}", level)?;
                                            Some(return_struct)
                                        }
                                        _ => None,
                                    };

                                    for (index, member) in members.iter().enumerate() {
                                        // TODO: handle builtin in better way
                                        if let Some(crate::Binding::BuiltIn(builtin)) =
                                            member.binding
                                        {
                                            match builtin {
                                                crate::BuiltIn::ClipDistance
                                                | crate::BuiltIn::CullDistance
                                                | crate::BuiltIn::PointSize => {
                                                    if self.options.version.is_es() {
                                                        continue;
                                                    }
                                                }
                                                _ => {}
                                            }
                                        }

                                        let varying_name = VaryingName {
                                            binding: member.binding.as_ref().unwrap(),
                                            stage: ep.stage,
                                            output: true,
                                        };
                                        write!(self.out, "{} = ", varying_name)?;

                                        if let Some(struct_name) = temp_struct_name {
                                            write!(self.out, "{}", struct_name)?;
                                        } else {
                                            self.write_expr(value, ctx)?;
                                        }

                                        // Write field name
                                        writeln!(
                                            self.out,
                                            ".{};",
                                            &self.names
                                                [&NameKey::StructMember(result.ty, index as u32)]
                                        )?;
                                        write!(self.out, "{}", level)?;
                                    }
                                }
                                _ => {
                                    let name = VaryingName {
                                        binding: result.binding.as_ref().unwrap(),
                                        stage: ep.stage,
                                        output: true,
                                    };
                                    write!(self.out, "{} = ", name)?;
                                    self.write_expr(value, ctx)?;
                                    writeln!(self.out, ";")?;
                                    write!(self.out, "{}", level)?;
                                }
                            }
                        }

                        if let back::FunctionType::EntryPoint(ep_index) = ctx.ty {
                            if self.module.entry_points[ep_index as usize].stage
                                == crate::ShaderStage::Vertex
                                && self
                                    .options
                                    .writer_flags
                                    .contains(WriterFlags::ADJUST_COORDINATE_SPACE)
                            {
                                writeln!(
                                    self.out,
                                    "gl_Position.yz = vec2(-gl_Position.y, gl_Position.z * 2.0 - gl_Position.w);",
                                )?;
                                write!(self.out, "{}", level)?;
                            }
                        }
                        writeln!(self.out, "return;")?;
                    }
                }
            }
            // This is one of the places were glsl adds to the syntax of C in this case the discard
            // keyword which ceases all further processing in a fragment shader, it's called OpKill
            // in spir-v that's why it's called `Statement::Kill`
            Statement::Kill => writeln!(self.out, "{}discard;", level)?,
            // Issue a memory barrier. Please note that to ensure visibility,
            // OpenGL always requires a call to the `barrier()` function after a `memoryBarrier*()`
            Statement::Barrier(flags) => {
                if flags.contains(crate::Barrier::STORAGE) {
                    writeln!(self.out, "{}memoryBarrierBuffer();", level)?;
                }

                if flags.contains(crate::Barrier::WORK_GROUP) {
                    writeln!(self.out, "{}memoryBarrierShared();", level)?;
                }

                writeln!(self.out, "{}barrier();", level)?;
            }
            // Stores in glsl are just variable assignments written as `pointer = value;`
            Statement::Store { pointer, value } => {
                write!(self.out, "{}", level)?;
                self.write_expr(pointer, ctx)?;
                write!(self.out, " = ")?;
                self.write_expr(value, ctx)?;
                writeln!(self.out, ";")?
            }
            // Stores a value into an image.
            Statement::ImageStore {
                image,
                coordinate,
                array_index,
                value,
            } => {
                write!(self.out, "{}", level)?;
                // This will only panic if the module is invalid
                let dim = match *ctx.info[image].ty.inner_with(&self.module.types) {
                    TypeInner::Image { dim, .. } => dim,
                    _ => unreachable!(),
                };

                write!(self.out, "imageStore(")?;
                self.write_expr(image, ctx)?;
                write!(self.out, ", ")?;
                self.write_texture_coordinates(coordinate, array_index, dim, ctx)?;
                write!(self.out, ", ")?;
                self.write_expr(value, ctx)?;
                writeln!(self.out, ");")?;
            }
            // A `Call` is written `name(arguments)` where `arguments` is a comma separated expressions list
            Statement::Call {
                function,
                ref arguments,
                result,
            } => {
                write!(self.out, "{}", level)?;
                if let Some(expr) = result {
                    let name = format!("{}{}", back::BAKE_PREFIX, expr.index());
                    let result = self.module.functions[function].result.as_ref().unwrap();
                    self.write_type(result.ty)?;
                    write!(self.out, " {} = ", name)?;
                    self.named_expressions.insert(expr, name);
                }
                write!(self.out, "{}(", &self.names[&NameKey::Function(function)])?;
                let arguments: Vec<_> = arguments
                    .iter()
                    .enumerate()
                    .filter_map(|(i, arg)| {
                        let arg_ty = self.module.functions[function].arguments[i].ty;
                        match self.module.types[arg_ty].inner {
                            TypeInner::Sampler { .. } => None,
                            _ => Some(*arg),
                        }
                    })
                    .collect();
                self.write_slice(&arguments, |this, _, arg| this.write_expr(*arg, ctx))?;
                writeln!(self.out, ");")?
            }
            Statement::Atomic {
                pointer,
                ref fun,
                value,
                result,
            } => {
                write!(self.out, "{}", level)?;
                let res_name = format!("{}{}", back::BAKE_PREFIX, result.index());
                let res_ty = ctx.info[result].ty.inner_with(&self.module.types);
                self.write_value_type(res_ty)?;
                write!(self.out, " {} = ", res_name)?;
                self.named_expressions.insert(result, res_name);

                let fun_str = fun.to_glsl();
                write!(self.out, "atomic{}(", fun_str)?;
                self.write_expr(pointer, ctx)?;
                write!(self.out, ", ")?;
                // handle the special cases
                match *fun {
                    crate::AtomicFunction::Subtract => {
                        // we just wrote `InterlockedAdd`, so negate the argument
                        write!(self.out, "-")?;
                    }
                    crate::AtomicFunction::Exchange { compare: Some(_) } => {
                        return Err(Error::Custom(
                            "atomic CompareExchange is not implemented".to_string(),
                        ));
                    }
                    _ => {}
                }
                self.write_expr(value, ctx)?;
                writeln!(self.out, ");")?;
            }
        }

        Ok(())
    }

    /// Helper method to write expressions
    ///
    /// # Notes
    /// Doesn't add any newlines or leading/trailing spaces
    fn write_expr(
        &mut self,
        expr: Handle<crate::Expression>,
        ctx: &back::FunctionCtx<'_>,
    ) -> BackendResult {
        use crate::Expression;

        if let Some(name) = self.named_expressions.get(&expr) {
            write!(self.out, "{}", name)?;
            return Ok(());
        }

        match ctx.expressions[expr] {
            // `Access` is applied to arrays, vectors and matrices and is written as indexing
            Expression::Access { base, index } => {
                self.write_expr(base, ctx)?;
                write!(self.out, "[")?;
                self.write_expr(index, ctx)?;
                write!(self.out, "]")?
            }
            // `AccessIndex` is the same as `Access` except that the index is a constant and it can
            // be applied to structs, in this case we need to find the name of the field at that
            // index and write `base.field_name`
            Expression::AccessIndex { base, index } => {
                self.write_expr(base, ctx)?;

                let base_ty_res = &ctx.info[base].ty;
                let mut resolved = base_ty_res.inner_with(&self.module.types);
                let base_ty_handle = match *resolved {
                    TypeInner::Pointer { base, space: _ } => {
                        resolved = &self.module.types[base].inner;
                        Some(base)
                    }
                    _ => base_ty_res.handle(),
                };

                match *resolved {
                    TypeInner::Vector { .. } => {
                        // Write vector access as a swizzle
                        write!(self.out, ".{}", back::COMPONENTS[index as usize])?
                    }
                    TypeInner::Matrix { .. }
                    | TypeInner::Array { .. }
                    | TypeInner::ValuePointer { .. } => write!(self.out, "[{}]", index)?,
                    TypeInner::Struct { .. } => {
                        // This will never panic in case the type is a `Struct`, this is not true
                        // for other types so we can only check while inside this match arm
                        let ty = base_ty_handle.unwrap();

                        write!(
                            self.out,
                            ".{}",
                            &self.names[&NameKey::StructMember(ty, index)]
                        )?
                    }
                    ref other => return Err(Error::Custom(format!("Cannot index {:?}", other))),
                }
            }
            // Constants are delegated to `write_constant`
            Expression::Constant(constant) => self.write_constant(constant)?,
            // `Splat` needs to actually write down a vector, it's not always inferred in GLSL.
            Expression::Splat { size: _, value } => {
                let resolved = ctx.info[expr].ty.inner_with(&self.module.types);
                self.write_value_type(resolved)?;
                write!(self.out, "(")?;
                self.write_expr(value, ctx)?;
                write!(self.out, ")")?
            }
            // `Swizzle` adds a few letters behind the dot.
            Expression::Swizzle {
                size,
                vector,
                pattern,
            } => {
                self.write_expr(vector, ctx)?;
                write!(self.out, ".")?;
                for &sc in pattern[..size as usize].iter() {
                    self.out.write_char(back::COMPONENTS[sc as usize])?;
                }
            }
            // `Compose` is pretty simple we just write `type(components)` where `components` is a
            // comma separated list of expressions
            Expression::Compose { ty, ref components } => {
                self.write_type(ty)?;

                let resolved = ctx.info[expr].ty.inner_with(&self.module.types);
                if let TypeInner::Array { base, size, .. } = *resolved {
                    self.write_array_size(base, size)?;
                }

                write!(self.out, "(")?;
                self.write_slice(components, |this, _, arg| this.write_expr(*arg, ctx))?;
                write!(self.out, ")")?
            }
            // Function arguments are written as the argument name
            Expression::FunctionArgument(pos) => {
                write!(self.out, "{}", &self.names[&ctx.argument_key(pos)])?
            }
            // Global variables need some special work for their name but
            // `get_global_name` does the work for us
            Expression::GlobalVariable(handle) => {
                let global = &self.module.global_variables[handle];
                self.write_global_name(handle, global)?
            }
            // A local is written as it's name
            Expression::LocalVariable(handle) => {
                write!(self.out, "{}", self.names[&ctx.name_key(handle)])?
            }
            // glsl has no pointers so there's no load operation, just write the pointer expression
            Expression::Load { pointer } => self.write_expr(pointer, ctx)?,
            // `ImageSample` is a bit complicated compared to the rest of the IR.
            //
            // First there are three variations depending whether the sample level is explicitly set,
            // if it's automatic or it it's bias:
            // `texture(image, coordinate)` - Automatic sample level
            // `texture(image, coordinate, bias)` - Bias sample level
            // `textureLod(image, coordinate, level)` - Zero or Exact sample level
            //
            // Furthermore if `depth_ref` is some we need to append it to the coordinate vector
            Expression::ImageSample {
                image,
                sampler: _, //TODO?
                gather,
                coordinate,
                array_index,
                offset,
                level,
                depth_ref,
            } => {
                let dim = match *ctx.info[image].ty.inner_with(&self.module.types) {
                    TypeInner::Image { dim, .. } => dim,
                    _ => unreachable!(),
                };

                if dim == crate::ImageDimension::Cube
                    && array_index.is_some()
                    && depth_ref.is_some()
                {
                    match level {
                        crate::SampleLevel::Zero
                        | crate::SampleLevel::Exact(_)
                        | crate::SampleLevel::Gradient { .. }
                        | crate::SampleLevel::Bias(_) => {
                            return Err(Error::Custom(String::from(
                                "gsamplerCubeArrayShadow isn't supported in textureGrad, \
                                 textureLod or texture with bias",
                            )))
                        }
                        crate::SampleLevel::Auto => {}
                    }
                }

                // textureLod on sampler2DArrayShadow and samplerCubeShadow does not exist in GLSL.
                // To emulate this, we will have to use textureGrad with a constant gradient of 0.
                let workaround_lod_array_shadow_as_grad = (array_index.is_some()
                    || dim == crate::ImageDimension::Cube)
                    && depth_ref.is_some()
                    && gather.is_none()
                    && !self
                        .options
                        .writer_flags
                        .contains(WriterFlags::TEXTURE_SHADOW_LOD);

                //Write the function to be used depending on the sample level
                let fun_name = match level {
                    crate::SampleLevel::Zero if gather.is_some() => "textureGather",
                    crate::SampleLevel::Auto | crate::SampleLevel::Bias(_) => "texture",
                    crate::SampleLevel::Zero | crate::SampleLevel::Exact(_) => {
                        if workaround_lod_array_shadow_as_grad {
                            "textureGrad"
                        } else {
                            "textureLod"
                        }
                    }
                    crate::SampleLevel::Gradient { .. } => "textureGrad",
                };
                let offset_name = match offset {
                    Some(_) => "Offset",
                    None => "",
                };

                write!(self.out, "{}{}(", fun_name, offset_name)?;

                // Write the image that will be used
                self.write_expr(image, ctx)?;
                // The space here isn't required but it helps with readability
                write!(self.out, ", ")?;

                // We need to get the coordinates vector size to later build a vector that's `size + 1`
                // if `depth_ref` is some, if it isn't a vector we panic as that's not a valid expression
                let mut coord_dim = match *ctx.info[coordinate].ty.inner_with(&self.module.types) {
                    TypeInner::Vector { size, .. } => size as u8,
                    TypeInner::Scalar { .. } => 1,
                    _ => unreachable!(),
                };

                if array_index.is_some() {
                    coord_dim += 1;
                }
                let merge_depth_ref = depth_ref.is_some() && gather.is_none() && coord_dim < 4;
                if merge_depth_ref {
                    coord_dim += 1;
                }

                let tex_1d_hack = dim == crate::ImageDimension::D1 && self.options.version.is_es();
                let is_vec = tex_1d_hack || coord_dim != 1;
                // Compose a new texture coordinates vector
                if is_vec {
                    write!(self.out, "vec{}(", coord_dim + tex_1d_hack as u8)?;
                }
                self.write_expr(coordinate, ctx)?;
                if tex_1d_hack {
                    write!(self.out, ", 0.0")?;
                }
                if let Some(expr) = array_index {
                    write!(self.out, ", ")?;
                    self.write_expr(expr, ctx)?;
                }
                if merge_depth_ref {
                    write!(self.out, ", ")?;
                    self.write_expr(depth_ref.unwrap(), ctx)?;
                }
                if is_vec {
                    write!(self.out, ")")?;
                }

                if let (Some(expr), false) = (depth_ref, merge_depth_ref) {
                    write!(self.out, ", ")?;
                    self.write_expr(expr, ctx)?;
                }

                match level {
                    // Auto needs no more arguments
                    crate::SampleLevel::Auto => (),
                    // Zero needs level set to 0
                    crate::SampleLevel::Zero => {
                        if workaround_lod_array_shadow_as_grad {
                            let vec_dim = match dim {
                                crate::ImageDimension::Cube => 3,
                                _ => 2,
                            };
                            write!(self.out, ", vec{}(0.0), vec{}(0.0)", vec_dim, vec_dim)?;
                        } else if gather.is_none() {
                            write!(self.out, ", 0.0")?;
                        }
                    }
                    // Exact and bias require another argument
                    crate::SampleLevel::Exact(expr) => {
                        if workaround_lod_array_shadow_as_grad {
                            log::warn!("Unable to `textureLod` a shadow array, ignoring the LOD");
                            write!(self.out, ", vec2(0,0), vec2(0,0)")?;
                        } else {
                            write!(self.out, ", ")?;
                            self.write_expr(expr, ctx)?;
                        }
                    }
                    crate::SampleLevel::Bias(_) => {
                        // This needs to be done after the offset writing
                    }
                    crate::SampleLevel::Gradient { x, y } => {
                        // If we are using sampler2D to replace sampler1D, we also
                        // need to make sure to use vec2 gradients
                        if tex_1d_hack {
                            write!(self.out, ", vec2(")?;
                            self.write_expr(x, ctx)?;
                            write!(self.out, ", 0.0)")?;
                            write!(self.out, ", vec2(")?;
                            self.write_expr(y, ctx)?;
                            write!(self.out, ", 0.0)")?;
                        } else {
                            write!(self.out, ", ")?;
                            self.write_expr(x, ctx)?;
                            write!(self.out, ", ")?;
                            self.write_expr(y, ctx)?;
                        }
                    }
                }

                if let Some(constant) = offset {
                    write!(self.out, ", ")?;
                    if tex_1d_hack {
                        write!(self.out, "ivec2(")?;
                    }
                    self.write_constant(constant)?;
                    if tex_1d_hack {
                        write!(self.out, ", 0)")?;
                    }
                }

                // Bias is always the last argument
                if let crate::SampleLevel::Bias(expr) = level {
                    write!(self.out, ", ")?;
                    self.write_expr(expr, ctx)?;
                }

                if let (Some(component), None) = (gather, depth_ref) {
                    write!(self.out, ", {}", component as usize)?;
                }

                // End the function
                write!(self.out, ")")?
            }
            // `ImageLoad` is also a bit complicated.
            // There are two functions one for sampled
            // images another for storage images, the former uses `texelFetch` and the latter uses
            // `imageLoad`.
            // Furthermore we have `index` which is always `Some` for sampled images
            // and `None` for storage images, so we end up with two functions:
            // `texelFetch(image, coordinate, index)` - for sampled images
            // `imageLoad(image, coordinate)` - for storage images
            Expression::ImageLoad {
                image,
                coordinate,
                array_index,
                sample,
                level,
            } => {
                // This will only panic if the module is invalid
                let (dim, class) = match *ctx.info[image].ty.inner_with(&self.module.types) {
                    TypeInner::Image {
                        dim,
                        arrayed: _,
                        class,
                    } => (dim, class),
                    _ => unreachable!(),
                };

                let fun_name = match class {
                    crate::ImageClass::Sampled { .. } => "texelFetch",
                    crate::ImageClass::Storage { .. } => "imageLoad",
                    // TODO: Is there even a function for this?
                    crate::ImageClass::Depth { multi: _ } => {
                        return Err(Error::Custom("TODO: depth sample loads".to_string()))
                    }
                };

                write!(self.out, "{}(", fun_name)?;
                self.write_expr(image, ctx)?;
                write!(self.out, ", ")?;
                self.write_texture_coordinates(coordinate, array_index, dim, ctx)?;

                if let Some(sample_or_level) = sample.or(level) {
                    write!(self.out, ", ")?;
                    self.write_expr(sample_or_level, ctx)?;
                }
                write!(self.out, ")")?;
            }
            // Query translates into one of the:
            // - textureSize/imageSize
            // - textureQueryLevels
            // - textureSamples/imageSamples
            Expression::ImageQuery { image, query } => {
                use crate::ImageClass;

                // This will only panic if the module is invalid
                let (dim, class) = match *ctx.info[image].ty.inner_with(&self.module.types) {
                    TypeInner::Image {
                        dim,
                        arrayed: _,
                        class,
                    } => (dim, class),
                    _ => unreachable!(),
                };
                let components = match dim {
                    crate::ImageDimension::D1 => 1,
                    crate::ImageDimension::D2 => 2,
                    crate::ImageDimension::D3 => 3,
                    crate::ImageDimension::Cube => 2,
                };
                match query {
                    crate::ImageQuery::Size { level } => {
                        match class {
                            ImageClass::Sampled { multi, .. } | ImageClass::Depth { multi } => {
                                write!(self.out, "textureSize(")?;
                                self.write_expr(image, ctx)?;
                                if let Some(expr) = level {
                                    write!(self.out, ", ")?;
                                    self.write_expr(expr, ctx)?;
                                } else if !multi {
                                    // All textureSize calls requires an lod argument
                                    // except for multisampled samplers
                                    write!(self.out, ", 0")?;
                                }
                            }
                            ImageClass::Storage { .. } => {
                                write!(self.out, "imageSize(")?;
                                self.write_expr(image, ctx)?;
                            }
                        }
                        write!(self.out, ")")?;
                        if components != 1 || self.options.version.is_es() {
                            write!(self.out, ".{}", &"xyz"[..components])?;
                        }
                    }
                    crate::ImageQuery::NumLevels => {
                        write!(self.out, "textureQueryLevels(",)?;
                        self.write_expr(image, ctx)?;
                        write!(self.out, ")",)?;
                    }
                    crate::ImageQuery::NumLayers => {
                        let fun_name = match class {
                            ImageClass::Sampled { .. } | ImageClass::Depth { .. } => "textureSize",
                            ImageClass::Storage { .. } => "imageSize",
                        };
                        write!(self.out, "{}(", fun_name)?;
                        self.write_expr(image, ctx)?;
                        // All textureSize calls requires an lod argument
                        // except for multisampled samplers
                        if class.is_multisampled() {
                            write!(self.out, ", 0")?;
                        }
                        write!(self.out, ")")?;
                        if components != 1 || self.options.version.is_es() {
                            write!(self.out, ".{}", back::COMPONENTS[components])?;
                        }
                    }
                    crate::ImageQuery::NumSamples => {
                        let fun_name = match class {
                            ImageClass::Sampled { .. } | ImageClass::Depth { .. } => {
                                "textureSamples"
                            }
                            ImageClass::Storage { .. } => "imageSamples",
                        };
                        write!(self.out, "{}(", fun_name)?;
                        self.write_expr(image, ctx)?;
                        write!(self.out, ")",)?;
                    }
                }
            }
            // `Unary` is pretty straightforward
            // "-" - for `Negate`
            // "~" - for `Not` if it's an integer
            // "!" - for `Not` if it's a boolean
            //
            // We also wrap the everything in parentheses to avoid precedence issues
            Expression::Unary { op, expr } => {
                use crate::{ScalarKind as Sk, UnaryOperator as Uo};

                let ty = ctx.info[expr].ty.inner_with(&self.module.types);

                match *ty {
                    TypeInner::Vector { kind: Sk::Bool, .. } => {
                        write!(self.out, "not(")?;
                    }
                    _ => {
                        let operator = match op {
                            Uo::Negate => "-",
                            Uo::Not => match ty.scalar_kind() {
                                Some(Sk::Sint) | Some(Sk::Uint) => "~",
                                Some(Sk::Bool) => "!",
                                ref other => {
                                    return Err(Error::Custom(format!(
                                        "Cannot apply not to type {:?}",
                                        other
                                    )))
                                }
                            },
                        };

                        write!(self.out, "({}", operator)?;
                    }
                }

                self.write_expr(expr, ctx)?;

                write!(self.out, ")")?
            }
            // `Binary` we just write `left op right`, except when dealing with
            // comparison operations on vectors as they are implemented with
            // builtin functions.
            // Once again we wrap everything in parentheses to avoid precedence issues
            Expression::Binary {
                mut op,
                left,
                right,
            } => {
                // Holds `Some(function_name)` if the binary operation is
                // implemented as a function call
                use crate::{BinaryOperator as Bo, ScalarKind as Sk, TypeInner as Ti};

                let left_inner = ctx.info[left].ty.inner_with(&self.module.types);
                let right_inner = ctx.info[right].ty.inner_with(&self.module.types);

                let function = match (left_inner, right_inner) {
                    (&Ti::Vector { kind, .. }, &Ti::Vector { .. }) => match op {
                        Bo::Less
                        | Bo::LessEqual
                        | Bo::Greater
                        | Bo::GreaterEqual
                        | Bo::Equal
                        | Bo::NotEqual => BinaryOperation::VectorCompare,
                        Bo::Modulo if kind == Sk::Float => BinaryOperation::Modulo,
                        Bo::And if kind == Sk::Bool => {
                            op = crate::BinaryOperator::LogicalAnd;
                            BinaryOperation::VectorComponentWise
                        }
                        Bo::InclusiveOr if kind == Sk::Bool => {
                            op = crate::BinaryOperator::LogicalOr;
                            BinaryOperation::VectorComponentWise
                        }
                        _ => BinaryOperation::Other,
                    },
                    _ => match (left_inner.scalar_kind(), right_inner.scalar_kind()) {
                        (Some(Sk::Float), _) | (_, Some(Sk::Float)) => match op {
                            Bo::Modulo => BinaryOperation::Modulo,
                            _ => BinaryOperation::Other,
                        },
                        (Some(Sk::Bool), Some(Sk::Bool)) => match op {
                            Bo::InclusiveOr => {
                                op = crate::BinaryOperator::LogicalOr;
                                BinaryOperation::Other
                            }
                            Bo::And => {
                                op = crate::BinaryOperator::LogicalAnd;
                                BinaryOperation::Other
                            }
                            _ => BinaryOperation::Other,
                        },
                        _ => BinaryOperation::Other,
                    },
                };

                match function {
                    BinaryOperation::VectorCompare => {
                        let op_str = match op {
                            Bo::Less => "lessThan(",
                            Bo::LessEqual => "lessThanEqual(",
                            Bo::Greater => "greaterThan(",
                            Bo::GreaterEqual => "greaterThanEqual(",
                            Bo::Equal => "equal(",
                            Bo::NotEqual => "notEqual(",
                            _ => unreachable!(),
                        };
                        write!(self.out, "{}", op_str)?;
                        self.write_expr(left, ctx)?;
                        write!(self.out, ", ")?;
                        self.write_expr(right, ctx)?;
                        write!(self.out, ")")?;
                    }
                    BinaryOperation::VectorComponentWise => {
                        self.write_value_type(left_inner)?;
                        write!(self.out, "(")?;

                        let size = match *left_inner {
                            Ti::Vector { size, .. } => size,
                            _ => unreachable!(),
                        };

                        for i in 0..size as usize {
                            if i != 0 {
                                write!(self.out, ", ")?;
                            }

                            self.write_expr(left, ctx)?;
                            write!(self.out, ".{}", back::COMPONENTS[i])?;

                            write!(self.out, " {} ", back::binary_operation_str(op))?;

                            self.write_expr(right, ctx)?;
                            write!(self.out, ".{}", back::COMPONENTS[i])?;
                        }

                        write!(self.out, ")")?;
                    }
                    // TODO: handle undefined behavior of BinaryOperator::Modulo
                    //
                    // sint:
                    // if right == 0 return 0
                    // if left == min(type_of(left)) && right == -1 return 0
                    // if sign(left) == -1 || sign(right) == -1 return result as defined by WGSL
                    //
                    // uint:
                    // if right == 0 return 0
                    //
                    // float:
                    // if right == 0 return ? see https://github.com/gpuweb/gpuweb/issues/2798
                    BinaryOperation::Modulo => {
                        write!(self.out, "(")?;

                        // write `e1 - e2 * trunc(e1 / e2)`
                        self.write_expr(left, ctx)?;
                        write!(self.out, " - ")?;
                        self.write_expr(right, ctx)?;
                        write!(self.out, " * ")?;
                        write!(self.out, "trunc(")?;
                        self.write_expr(left, ctx)?;
                        write!(self.out, " / ")?;
                        self.write_expr(right, ctx)?;
                        write!(self.out, ")")?;

                        write!(self.out, ")")?;
                    }
                    BinaryOperation::Other => {
                        write!(self.out, "(")?;

                        self.write_expr(left, ctx)?;
                        write!(self.out, " {} ", back::binary_operation_str(op))?;
                        self.write_expr(right, ctx)?;

                        write!(self.out, ")")?;
                    }
                }
            }
            // `Select` is written as `condition ? accept : reject`
            // We wrap everything in parentheses to avoid precedence issues
            Expression::Select {
                condition,
                accept,
                reject,
            } => {
                let cond_ty = ctx.info[condition].ty.inner_with(&self.module.types);
                let vec_select = if let TypeInner::Vector { .. } = *cond_ty {
                    true
                } else {
                    false
                };

                // TODO: Boolean mix on desktop required GL_EXT_shader_integer_mix
                if vec_select {
                    // Glsl defines that for mix when the condition is a boolean the first element
                    // is picked if condition is false and the second if condition is true
                    write!(self.out, "mix(")?;
                    self.write_expr(reject, ctx)?;
                    write!(self.out, ", ")?;
                    self.write_expr(accept, ctx)?;
                    write!(self.out, ", ")?;
                    self.write_expr(condition, ctx)?;
                } else {
                    write!(self.out, "(")?;
                    self.write_expr(condition, ctx)?;
                    write!(self.out, " ? ")?;
                    self.write_expr(accept, ctx)?;
                    write!(self.out, " : ")?;
                    self.write_expr(reject, ctx)?;
                }

                write!(self.out, ")")?
            }
            // `Derivative` is a function call to a glsl provided function
            Expression::Derivative { axis, expr } => {
                use crate::DerivativeAxis as Da;

                write!(
                    self.out,
                    "{}(",
                    match axis {
                        Da::X => "dFdx",
                        Da::Y => "dFdy",
                        Da::Width => "fwidth",
                    }
                )?;
                self.write_expr(expr, ctx)?;
                write!(self.out, ")")?
            }
            // `Relational` is a normal function call to some glsl provided functions
            Expression::Relational { fun, argument } => {
                use crate::RelationalFunction as Rf;

                let fun_name = match fun {
                    // There's no specific function for this but we can invert the result of `isinf`
                    Rf::IsFinite => "!isinf",
                    Rf::IsInf => "isinf",
                    Rf::IsNan => "isnan",
                    // There's also no function for this but we can invert `isnan`
                    Rf::IsNormal => "!isnan",
                    Rf::All => "all",
                    Rf::Any => "any",
                };
                write!(self.out, "{}(", fun_name)?;

                self.write_expr(argument, ctx)?;

                write!(self.out, ")")?
            }
            Expression::Math {
                fun,
                arg,
                arg1,
                arg2,
                arg3,
            } => {
                use crate::MathFunction as Mf;

                let fun_name = match fun {
                    // comparison
                    Mf::Abs => "abs",
                    Mf::Min => "min",
                    Mf::Max => "max",
                    Mf::Clamp => "clamp",
                    // trigonometry
                    Mf::Cos => "cos",
                    Mf::Cosh => "cosh",
                    Mf::Sin => "sin",
                    Mf::Sinh => "sinh",
                    Mf::Tan => "tan",
                    Mf::Tanh => "tanh",
                    Mf::Acos => "acos",
                    Mf::Asin => "asin",
                    Mf::Atan => "atan",
                    Mf::Asinh => "asinh",
                    Mf::Acosh => "acosh",
                    Mf::Atanh => "atanh",
                    Mf::Radians => "radians",
                    Mf::Degrees => "degrees",
                    // glsl doesn't have atan2 function
                    // use two-argument variation of the atan function
                    Mf::Atan2 => "atan",
                    // decomposition
                    Mf::Ceil => "ceil",
                    Mf::Floor => "floor",
                    Mf::Round => "roundEven",
                    Mf::Fract => "fract",
                    Mf::Trunc => "trunc",
                    Mf::Modf => "modf",
                    Mf::Frexp => "frexp",
                    Mf::Ldexp => "ldexp",
                    // exponent
                    Mf::Exp => "exp",
                    Mf::Exp2 => "exp2",
                    Mf::Log => "log",
                    Mf::Log2 => "log2",
                    Mf::Pow => "pow",
                    // geometry
                    Mf::Dot => match *ctx.info[arg].ty.inner_with(&self.module.types) {
                        crate::TypeInner::Vector {
                            kind: crate::ScalarKind::Float,
                            ..
                        } => "dot",
                        crate::TypeInner::Vector { size, .. } => {
                            return self.write_dot_product(arg, arg1.unwrap(), size as usize)
                        }
                        _ => unreachable!(
                            "Correct TypeInner for dot product should be already validated"
                        ),
                    },
                    Mf::Outer => "outerProduct",
                    Mf::Cross => "cross",
                    Mf::Distance => "distance",
                    Mf::Length => "length",
                    Mf::Normalize => "normalize",
                    Mf::FaceForward => "faceforward",
                    Mf::Reflect => "reflect",
                    Mf::Refract => "refract",
                    // computational
                    Mf::Sign => "sign",
                    Mf::Fma => {
                        if self.options.version.supports_fma_function() {
                            // Use the fma function when available
                            "fma"
                        } else {
                            // No fma support. Transform the function call into an arithmetic expression
                            write!(self.out, "(")?;

                            self.write_expr(arg, ctx)?;
                            write!(self.out, " * ")?;

                            let arg1 =
                                arg1.ok_or_else(|| Error::Custom("Missing fma arg1".to_owned()))?;
                            self.write_expr(arg1, ctx)?;
                            write!(self.out, " + ")?;

                            let arg2 =
                                arg2.ok_or_else(|| Error::Custom("Missing fma arg2".to_owned()))?;
                            self.write_expr(arg2, ctx)?;
                            write!(self.out, ")")?;

                            return Ok(());
                        }
                    }
                    Mf::Mix => "mix",
                    Mf::Step => "step",
                    Mf::SmoothStep => "smoothstep",
                    Mf::Sqrt => "sqrt",
                    Mf::InverseSqrt => "inversesqrt",
                    Mf::Inverse => "inverse",
                    Mf::Transpose => "transpose",
                    Mf::Determinant => "determinant",
                    // bits
                    Mf::CountOneBits => "bitCount",
                    Mf::ReverseBits => "bitfieldReverse",
                    Mf::ExtractBits => "bitfieldExtract",
                    Mf::InsertBits => "bitfieldInsert",
                    Mf::FindLsb => "findLSB",
                    Mf::FindMsb => "findMSB",
                    // data packing
                    Mf::Pack4x8snorm => "packSnorm4x8",
                    Mf::Pack4x8unorm => "packUnorm4x8",
                    Mf::Pack2x16snorm => "packSnorm2x16",
                    Mf::Pack2x16unorm => "packUnorm2x16",
                    Mf::Pack2x16float => "packHalf2x16",
                    // data unpacking
                    Mf::Unpack4x8snorm => "unpackSnorm4x8",
                    Mf::Unpack4x8unorm => "unpackUnorm4x8",
                    Mf::Unpack2x16snorm => "unpackSnorm2x16",
                    Mf::Unpack2x16unorm => "unpackUnorm2x16",
                    Mf::Unpack2x16float => "unpackHalf2x16",
                };

                let extract_bits = fun == Mf::ExtractBits;
                let insert_bits = fun == Mf::InsertBits;

                // we might need to cast to unsigned integers since
                // GLSL's findLSB / findMSB always return signed integers
                let need_extra_paren = {
                    (fun == Mf::FindLsb || fun == Mf::FindMsb || fun == Mf::CountOneBits)
                        && match *ctx.info[arg].ty.inner_with(&self.module.types) {
                            crate::TypeInner::Scalar {
                                kind: crate::ScalarKind::Uint,
                                ..
                            } => {
                                write!(self.out, "uint(")?;
                                true
                            }
                            crate::TypeInner::Vector {
                                kind: crate::ScalarKind::Uint,
                                size,
                                ..
                            } => {
                                write!(self.out, "uvec{}(", size as u8)?;
                                true
                            }
                            _ => false,
                        }
                };

                write!(self.out, "{}(", fun_name)?;
                self.write_expr(arg, ctx)?;
                if let Some(arg) = arg1 {
                    write!(self.out, ", ")?;
                    if extract_bits {
                        write!(self.out, "int(")?;
                        self.write_expr(arg, ctx)?;
                        write!(self.out, ")")?;
                    } else {
                        self.write_expr(arg, ctx)?;
                    }
                }
                if let Some(arg) = arg2 {
                    write!(self.out, ", ")?;
                    if extract_bits || insert_bits {
                        write!(self.out, "int(")?;
                        self.write_expr(arg, ctx)?;
                        write!(self.out, ")")?;
                    } else {
                        self.write_expr(arg, ctx)?;
                    }
                }
                if let Some(arg) = arg3 {
                    write!(self.out, ", ")?;
                    if insert_bits {
                        write!(self.out, "int(")?;
                        self.write_expr(arg, ctx)?;
                        write!(self.out, ")")?;
                    } else {
                        self.write_expr(arg, ctx)?;
                    }
                }
                write!(self.out, ")")?;

                if need_extra_paren {
                    write!(self.out, ")")?
                }
            }
            // `As` is always a call.
            // If `convert` is true the function name is the type
            // Else the function name is one of the glsl provided bitcast functions
            Expression::As {
                expr,
                kind: target_kind,
                convert,
            } => {
                let inner = ctx.info[expr].ty.inner_with(&self.module.types);
                match convert {
                    Some(width) => {
                        // this is similar to `write_type`, but with the target kind
                        let scalar = glsl_scalar(target_kind, width)?;
                        match *inner {
                            TypeInner::Matrix { columns, rows, .. } => write!(
                                self.out,
                                "{}mat{}x{}",
                                scalar.prefix, columns as u8, rows as u8
                            )?,
                            TypeInner::Vector { size, .. } => {
                                write!(self.out, "{}vec{}", scalar.prefix, size as u8)?
                            }
                            _ => write!(self.out, "{}", scalar.full)?,
                        }

                        write!(self.out, "(")?;
                        self.write_expr(expr, ctx)?;
                        write!(self.out, ")")?
                    }
                    None => {
                        use crate::ScalarKind as Sk;

                        let source_kind = inner.scalar_kind().unwrap();
                        let conv_op = match (source_kind, target_kind) {
                            (Sk::Float, Sk::Sint) => "floatBitsToInt",
                            (Sk::Float, Sk::Uint) => "floatBitsToUint",
                            (Sk::Sint, Sk::Float) => "intBitsToFloat",
                            (Sk::Uint, Sk::Float) => "uintBitsToFloat",
                            // There is no way to bitcast between Uint/Sint in glsl. Use constructor conversion
                            (Sk::Uint, Sk::Sint) => "int",
                            (Sk::Sint, Sk::Uint) => "uint",

                            (Sk::Bool, Sk::Sint) => "int",
                            (Sk::Bool, Sk::Uint) => "uint",
                            (Sk::Bool, Sk::Float) => "float",

                            (Sk::Sint, Sk::Bool) => "bool",
                            (Sk::Uint, Sk::Bool) => "bool",
                            (Sk::Float, Sk::Bool) => "bool",

                            // No conversion needed
                            (Sk::Sint, Sk::Sint) => "",
                            (Sk::Uint, Sk::Uint) => "",
                            (Sk::Float, Sk::Float) => "",
                            (Sk::Bool, Sk::Bool) => "",
                        };
                        write!(self.out, "{}", conv_op)?;
                        if !conv_op.is_empty() {
                            write!(self.out, "(")?;
                        }
                        self.write_expr(expr, ctx)?;
                        if !conv_op.is_empty() {
                            write!(self.out, ")")?
                        }
                    }
                }
            }
            // These expressions never show up in `Emit`.
            Expression::CallResult(_) | Expression::AtomicResult { .. } => unreachable!(),
            // `ArrayLength` is written as `expr.length()` and we convert it to a uint
            Expression::ArrayLength(expr) => {
                write!(self.out, "uint(")?;
                self.write_expr(expr, ctx)?;
                write!(self.out, ".length())")?
            }
        }

        Ok(())
    }

    fn write_texture_coordinates(
        &mut self,
        coordinate: Handle<crate::Expression>,
        array_index: Option<Handle<crate::Expression>>,
        dim: crate::ImageDimension,
        ctx: &back::FunctionCtx,
    ) -> Result<(), Error> {
        use crate::ImageDimension as IDim;

        let tex_1d_hack = dim == IDim::D1 && self.options.version.is_es();
        match array_index {
            Some(layer_expr) => {
                let tex_coord_size = match dim {
                    IDim::D1 => 2,
                    IDim::D2 => 3,
                    IDim::D3 => 4,
                    IDim::Cube => 4,
                };
                write!(self.out, "ivec{}(", tex_coord_size + tex_1d_hack as u8)?;
                self.write_expr(coordinate, ctx)?;
                write!(self.out, ", ")?;
                // If we are replacing sampler1D with sampler2D we also need
                // to add another zero to the coordinates vector for the y component
                if tex_1d_hack {
                    write!(self.out, "0, ")?;
                }
                self.write_expr(layer_expr, ctx)?;
                write!(self.out, ")")?;
            }
            None => {
                if tex_1d_hack {
                    write!(self.out, "ivec2(")?;
                }
                self.write_expr(coordinate, ctx)?;
                if tex_1d_hack {
                    write!(self.out, ", 0.0)")?;
                }
            }
        }
        Ok(())
    }

    fn write_named_expr(
        &mut self,
        handle: Handle<crate::Expression>,
        name: String,
        ctx: &back::FunctionCtx,
    ) -> BackendResult {
        match ctx.info[handle].ty {
            proc::TypeResolution::Handle(ty_handle) => match self.module.types[ty_handle].inner {
                TypeInner::Struct { .. } => {
                    let ty_name = &self.names[&NameKey::Type(ty_handle)];
                    write!(self.out, "{}", ty_name)?;
                }
                _ => {
                    self.write_type(ty_handle)?;
                }
            },
            proc::TypeResolution::Value(ref inner) => {
                self.write_value_type(inner)?;
            }
        }

        let base_ty_res = &ctx.info[handle].ty;
        let resolved = base_ty_res.inner_with(&self.module.types);

        write!(self.out, " {}", name)?;
        if let TypeInner::Array { base, size, .. } = *resolved {
            self.write_array_size(base, size)?;
        }
        write!(self.out, " = ")?;
        self.write_expr(handle, ctx)?;
        writeln!(self.out, ";")?;
        self.named_expressions.insert(handle, name);

        Ok(())
    }

    /// Helper function that write string with default zero initialization for supported types
    fn write_zero_init_value(&mut self, ty: Handle<crate::Type>) -> BackendResult {
        let inner = &self.module.types[ty].inner;
        match *inner {
            TypeInner::Scalar { kind, .. } => {
                self.write_zero_init_scalar(kind)?;
            }
            TypeInner::Vector { kind, .. } => {
                self.write_value_type(inner)?;
                write!(self.out, "(")?;
                self.write_zero_init_scalar(kind)?;
                write!(self.out, ")")?;
            }
            TypeInner::Matrix { .. } => {
                self.write_value_type(inner)?;
                write!(self.out, "(")?;
                self.write_zero_init_scalar(crate::ScalarKind::Float)?;
                write!(self.out, ")")?;
            }
            TypeInner::Array { base, size, .. } => {
                let count = match size
                    .to_indexable_length(self.module)
                    .expect("Bad array size")
                {
                    proc::IndexableLength::Known(count) => count,
                    proc::IndexableLength::Dynamic => return Ok(()),
                };
                self.write_type(base)?;
                self.write_array_size(base, size)?;
                write!(self.out, "(")?;
                for _ in 1..count {
                    self.write_zero_init_value(base)?;
                    write!(self.out, ", ")?;
                }
                // write last parameter without comma and space
                self.write_zero_init_value(base)?;
                write!(self.out, ")")?;
            }
            TypeInner::Struct { ref members, .. } => {
                let name = &self.names[&NameKey::Type(ty)];
                write!(self.out, "{}(", name)?;
                for (i, member) in members.iter().enumerate() {
                    self.write_zero_init_value(member.ty)?;
                    if i != members.len().saturating_sub(1) {
                        write!(self.out, ", ")?;
                    }
                }
                write!(self.out, ")")?;
            }
            _ => {} // TODO:
        }

        Ok(())
    }

    /// Helper function that write string with zero initialization for scalar
    fn write_zero_init_scalar(&mut self, kind: crate::ScalarKind) -> BackendResult {
        match kind {
            crate::ScalarKind::Bool => write!(self.out, "false")?,
            crate::ScalarKind::Uint => write!(self.out, "0u")?,
            crate::ScalarKind::Float => write!(self.out, "0.0")?,
            crate::ScalarKind::Sint => write!(self.out, "0")?,
        }

        Ok(())
    }

    /// Helper function that return the glsl storage access string of [`StorageAccess`](crate::StorageAccess)
    ///
    /// glsl allows adding both `readonly` and `writeonly` but this means that
    /// they can only be used to query information about the resource which isn't what
    /// we want here so when storage access is both `LOAD` and `STORE` add no modifiers
    fn write_storage_access(&mut self, storage_access: crate::StorageAccess) -> BackendResult {
        if !storage_access.contains(crate::StorageAccess::STORE) {
            write!(self.out, "readonly ")?;
        }
        if !storage_access.contains(crate::StorageAccess::LOAD) {
            write!(self.out, "writeonly ")?;
        }
        Ok(())
    }

    /// Helper method used to produce the reflection info that's returned to the user
    fn collect_reflection_info(&self) -> Result<ReflectionInfo, Error> {
        use std::collections::hash_map::Entry;
        let info = self.info.get_entry_point(self.entry_point_idx as usize);
        let mut texture_mapping = crate::FastHashMap::default();
        let mut uniforms = crate::FastHashMap::default();

        for sampling in info.sampling_set.iter() {
            let tex_name = self.reflection_names_globals[&sampling.image].clone();

            match texture_mapping.entry(tex_name) {
                Entry::Vacant(v) => {
                    v.insert(TextureMapping {
                        texture: sampling.image,
                        sampler: Some(sampling.sampler),
                    });
                }
                Entry::Occupied(e) => {
                    if e.get().sampler != Some(sampling.sampler) {
                        log::error!("Conflicting samplers for {}", e.key());
                        return Err(Error::ImageMultipleSamplers);
                    }
                }
            }
        }

        for (handle, var) in self.module.global_variables.iter() {
            if info[handle].is_empty() {
                continue;
            }
            match self.module.types[var.ty].inner {
                crate::TypeInner::Struct { .. } => match var.space {
                    crate::AddressSpace::Uniform | crate::AddressSpace::Storage { .. } => {
                        let name = self.reflection_names_globals[&handle].clone();
                        uniforms.insert(handle, name);
                    }
                    _ => (),
                },
                crate::TypeInner::Image { .. } => {
                    let tex_name = self.reflection_names_globals[&handle].clone();
                    match texture_mapping.entry(tex_name) {
                        Entry::Vacant(v) => {
                            v.insert(TextureMapping {
                                texture: handle,
                                sampler: None,
                            });
                        }
                        Entry::Occupied(_) => {
                            // already used with a sampler, do nothing
                        }
                    }
                }
                _ => {}
            }
        }

        Ok(ReflectionInfo {
            texture_mapping,
            uniforms,
        })
    }
}

/// Structure returned by [`glsl_scalar`](glsl_scalar)
///
/// It contains both a prefix used in other types and the full type name
struct ScalarString<'a> {
    /// The prefix used to compose other types
    prefix: &'a str,
    /// The name of the scalar type
    full: &'a str,
}

/// Helper function that returns scalar related strings
///
/// Check [`ScalarString`](ScalarString) for the information provided
///
/// # Errors
/// If a [`Float`](crate::ScalarKind::Float) with an width that isn't 4 or 8
const fn glsl_scalar(
    kind: crate::ScalarKind,
    width: crate::Bytes,
) -> Result<ScalarString<'static>, Error> {
    use crate::ScalarKind as Sk;

    Ok(match kind {
        Sk::Sint => ScalarString {
            prefix: "i",
            full: "int",
        },
        Sk::Uint => ScalarString {
            prefix: "u",
            full: "uint",
        },
        Sk::Float => match width {
            4 => ScalarString {
                prefix: "",
                full: "float",
            },
            8 => ScalarString {
                prefix: "d",
                full: "double",
            },
            _ => return Err(Error::UnsupportedScalar(kind, width)),
        },
        Sk::Bool => ScalarString {
            prefix: "b",
            full: "bool",
        },
    })
}

/// Helper function that returns the glsl variable name for a builtin
const fn glsl_built_in(built_in: crate::BuiltIn, output: bool) -> &'static str {
    use crate::BuiltIn as Bi;

    match built_in {
        Bi::Position { .. } => {
            if output {
                "gl_Position"
            } else {
                "gl_FragCoord"
            }
        }
        Bi::ViewIndex => "gl_ViewIndex",
        // vertex
        Bi::BaseInstance => "uint(gl_BaseInstance)",
        Bi::BaseVertex => "uint(gl_BaseVertex)",
        Bi::ClipDistance => "gl_ClipDistance",
        Bi::CullDistance => "gl_CullDistance",
        Bi::InstanceIndex => "uint(gl_InstanceID)",
        Bi::PointSize => "gl_PointSize",
        Bi::VertexIndex => "uint(gl_VertexID)",
        // fragment
        Bi::FragDepth => "gl_FragDepth",
        Bi::FrontFacing => "gl_FrontFacing",
        Bi::PrimitiveIndex => "uint(gl_PrimitiveID)",
        Bi::SampleIndex => "gl_SampleID",
        Bi::SampleMask => {
            if output {
                "gl_SampleMask"
            } else {
                "gl_SampleMaskIn"
            }
        }
        // compute
        Bi::GlobalInvocationId => "gl_GlobalInvocationID",
        Bi::LocalInvocationId => "gl_LocalInvocationID",
        Bi::LocalInvocationIndex => "gl_LocalInvocationIndex",
        Bi::WorkGroupId => "gl_WorkGroupID",
        Bi::WorkGroupSize => "gl_WorkGroupSize",
        Bi::NumWorkGroups => "gl_NumWorkGroups",
    }
}

/// Helper function that returns the string corresponding to the address space
const fn glsl_storage_qualifier(space: crate::AddressSpace) -> Option<&'static str> {
    use crate::AddressSpace as As;

    match space {
        As::Function => None,
        As::Private => None,
        As::Storage { .. } => Some("buffer"),
        As::Uniform => Some("uniform"),
        As::Handle => Some("uniform"),
        As::WorkGroup => Some("shared"),
        As::PushConstant => Some("uniform"),
    }
}

/// Helper function that returns the string corresponding to the glsl interpolation qualifier
const fn glsl_interpolation(interpolation: crate::Interpolation) -> &'static str {
    use crate::Interpolation as I;

    match interpolation {
        I::Perspective => "smooth",
        I::Linear => "noperspective",
        I::Flat => "flat",
    }
}

/// Return the GLSL auxiliary qualifier for the given sampling value.
const fn glsl_sampling(sampling: crate::Sampling) -> Option<&'static str> {
    use crate::Sampling as S;

    match sampling {
        S::Center => None,
        S::Centroid => Some("centroid"),
        S::Sample => Some("sample"),
    }
}

/// Helper function that returns the glsl dimension string of [`ImageDimension`](crate::ImageDimension)
const fn glsl_dimension(dim: crate::ImageDimension) -> &'static str {
    use crate::ImageDimension as IDim;

    match dim {
        IDim::D1 => "1D",
        IDim::D2 => "2D",
        IDim::D3 => "3D",
        IDim::Cube => "Cube",
    }
}

/// Helper function that returns the glsl storage format string of [`StorageFormat`](crate::StorageFormat)
const fn glsl_storage_format(format: crate::StorageFormat) -> &'static str {
    use crate::StorageFormat as Sf;

    match format {
        Sf::R8Unorm => "r8",
        Sf::R8Snorm => "r8_snorm",
        Sf::R8Uint => "r8ui",
        Sf::R8Sint => "r8i",
        Sf::R16Uint => "r16ui",
        Sf::R16Sint => "r16i",
        Sf::R16Float => "r16f",
        Sf::Rg8Unorm => "rg8",
        Sf::Rg8Snorm => "rg8_snorm",
        Sf::Rg8Uint => "rg8ui",
        Sf::Rg8Sint => "rg8i",
        Sf::R32Uint => "r32ui",
        Sf::R32Sint => "r32i",
        Sf::R32Float => "r32f",
        Sf::Rg16Uint => "rg16ui",
        Sf::Rg16Sint => "rg16i",
        Sf::Rg16Float => "rg16f",
        Sf::Rgba8Unorm => "rgba8ui",
        Sf::Rgba8Snorm => "rgba8_snorm",
        Sf::Rgba8Uint => "rgba8ui",
        Sf::Rgba8Sint => "rgba8i",
        Sf::Rgb10a2Unorm => "rgb10_a2ui",
        Sf::Rg11b10Float => "r11f_g11f_b10f",
        Sf::Rg32Uint => "rg32ui",
        Sf::Rg32Sint => "rg32i",
        Sf::Rg32Float => "rg32f",
        Sf::Rgba16Uint => "rgba16ui",
        Sf::Rgba16Sint => "rgba16i",
        Sf::Rgba16Float => "rgba16f",
        Sf::Rgba32Uint => "rgba32ui",
        Sf::Rgba32Sint => "rgba32i",
        Sf::Rgba32Float => "rgba32f",
    }
}

fn is_value_init_supported(module: &crate::Module, ty: Handle<crate::Type>) -> bool {
    match module.types[ty].inner {
        TypeInner::Scalar { .. } | TypeInner::Vector { .. } | TypeInner::Matrix { .. } => true,
        TypeInner::Array { base, size, .. } => {
            size != crate::ArraySize::Dynamic && is_value_init_supported(module, base)
        }
        TypeInner::Struct { ref members, .. } => members
            .iter()
            .all(|member| is_value_init_supported(module, member.ty)),
        _ => false,
    }
}
