#![allow(dead_code)]

use std::result::Result;

use nom::IResult;

use mem_arena::MemArena;

use color::{XYZ, rec709_e_to_xyz};
use shading::{SurfaceShader, SimpleSurfaceShader};

use super::basics::ws_f32;
use super::DataTree;
use super::psy::PsyParseError;


// pub struct TriangleMesh {
//    time_samples: usize,
//    geo: Vec<(Point, Point, Point)>,
//    indices: Vec<usize>,
//    accel: BVH,
// }

pub fn parse_surface_shader<'a>(
    arena: &'a MemArena,
    tree: &'a DataTree,
) -> Result<&'a SurfaceShader, PsyParseError> {
    let type_name = if let Some((_, text, _)) = tree.iter_leaf_children_with_type("Type").nth(0) {
        text.trim()
    } else {
        return Err(PsyParseError::MissingNode(
            tree.byte_offset(),
            "Expected a Type field in SurfaceShader.",
        ));
    };

    let shader = match type_name {
        "Emit" => {
            let color = if let Some((_, contents, byte_offset)) =
                tree.iter_leaf_children_with_type("Color").nth(0)
            {
                if let IResult::Done(_, color) =
                    closure!(tuple!(ws_f32, ws_f32, ws_f32))(contents.as_bytes())
                {
                    // TODO: handle color space conversions properly.
                    // Probably will need a special color type with its
                    // own parser...?
                    XYZ::from_tuple(rec709_e_to_xyz(color))
                } else {
                    // Found color, but its contents is not in the right format
                    return Err(PsyParseError::UnknownError(byte_offset));
                }
            } else {
                return Err(PsyParseError::MissingNode(
                    tree.byte_offset(),
                    "Expected a Color field in Emit SurfaceShader.",
                ));
            };

            arena.alloc(SimpleSurfaceShader::Emit { color: color })
        }
        "Lambert" => {
            let color = if let Some((_, contents, byte_offset)) =
                tree.iter_leaf_children_with_type("Color").nth(0)
            {
                if let IResult::Done(_, color) =
                    closure!(tuple!(ws_f32, ws_f32, ws_f32))(contents.as_bytes())
                {
                    // TODO: handle color space conversions properly.
                    // Probably will need a special color type with its
                    // own parser...?
                    XYZ::from_tuple(rec709_e_to_xyz(color))
                } else {
                    // Found color, but its contents is not in the right format
                    return Err(PsyParseError::UnknownError(byte_offset));
                }
            } else {
                return Err(PsyParseError::MissingNode(
                    tree.byte_offset(),
                    "Expected a Color field in Lambert SurfaceShader.",
                ));
            };

            arena.alloc(SimpleSurfaceShader::Lambert { color: color })
        }
        "GTR" => {
            // Color
            let color = if let Some((_, contents, byte_offset)) =
                tree.iter_leaf_children_with_type("Color").nth(0)
            {
                if let IResult::Done(_, color) =
                    closure!(tuple!(ws_f32, ws_f32, ws_f32))(contents.as_bytes())
                {
                    // TODO: handle color space conversions properly.
                    // Probably will need a special color type with its
                    // own parser...?
                    XYZ::from_tuple(rec709_e_to_xyz(color))
                } else {
                    // Found color, but its contents is not in the right format
                    return Err(PsyParseError::UnknownError(byte_offset));
                }
            } else {
                return Err(PsyParseError::MissingNode(
                    tree.byte_offset(),
                    "Expected a Color field in GTR SurfaceShader.",
                ));
            };

            // Roughness
            let roughness = if let Some((_, contents, byte_offset)) =
                tree.iter_leaf_children_with_type("Roughness").nth(0)
            {
                if let IResult::Done(_, roughness) = ws_f32(contents.as_bytes()) {
                    roughness
                } else {
                    return Err(PsyParseError::UnknownError(byte_offset));
                }
            } else {
                return Err(PsyParseError::MissingNode(
                    tree.byte_offset(),
                    "Expected a Roughness field in GTR SurfaceShader.",
                ));
            };

            // TailShape
            let tail_shape = if let Some((_, contents, byte_offset)) =
                tree.iter_leaf_children_with_type("TailShape").nth(0)
            {
                if let IResult::Done(_, tail_shape) = ws_f32(contents.as_bytes()) {
                    tail_shape
                } else {
                    return Err(PsyParseError::UnknownError(byte_offset));
                }
            } else {
                return Err(PsyParseError::MissingNode(
                    tree.byte_offset(),
                    "Expected a TailShape field in GTR SurfaceShader.",
                ));
            };

            // Fresnel
            let fresnel = if let Some((_, contents, byte_offset)) =
                tree.iter_leaf_children_with_type("Fresnel").nth(0)
            {
                if let IResult::Done(_, fresnel) = ws_f32(contents.as_bytes()) {
                    fresnel
                } else {
                    return Err(PsyParseError::UnknownError(byte_offset));
                }
            } else {
                return Err(PsyParseError::MissingNode(
                    tree.byte_offset(),
                    "Expected a Fresnel field in GTR SurfaceShader.",
                ));
            };

            arena.alloc(SimpleSurfaceShader::GTR {
                color: color,
                roughness: roughness,
                tail_shape: tail_shape,
                fresnel: fresnel,
            })
        }
        _ => unimplemented!(),
    };

    Ok(shader)
}
