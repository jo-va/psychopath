#![allow(dead_code)]

use std::result::Result;

use nom::IResult;

use super::DataTree;
use super::basics::{ws_usize, ws_f32};
use super::psy::PsyParseError;

use light::SphereLight;
use math::Point;
use color::XYZ;

pub fn parse_sphere_light(tree: &DataTree) -> Result<SphereLight, PsyParseError> {
    if let &DataTree::Internal { ref children, .. } = tree {
        let mut radii = Vec::new();
        let mut colors = Vec::new();

        // Parse
        for child in children.iter() {
            match child {
                // Radius
                &DataTree::Leaf { type_name, contents } if type_name == "Radius" => {
                    if let IResult::Done(_, radius) = ws_f32(contents.as_bytes()) {
                        radii.push(radius);
                    } else {
                        // Found radius, but its contents is not in the right format
                        return Err(PsyParseError::UnknownError);
                    }
                }

                // Color
                &DataTree::Leaf { type_name, contents } if type_name == "Color" => {
                    if let IResult::Done(_, color) = closure!(tuple!(ws_f32,
                                                                     ws_f32,
                                                                     ws_f32))(contents.as_bytes()) {
                        // TODO: handle color space conversions properly.
                        // Probably will need a special color type with its
                        // own parser...?
                        colors.push(XYZ::new(color.0, color.1, color.2));
                    } else {
                        // Found color, but its contents is not in the right format
                        return Err(PsyParseError::UnknownError);
                    }
                }

                _ => {}
            }
        }

        return Ok(SphereLight::new(radii, colors));
    } else {
        return Err(PsyParseError::UnknownError);
    }
}