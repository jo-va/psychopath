#![allow(dead_code)]

use bbox::BBox;


pub trait Boundable {
    fn bounds(&self) -> &[BBox];
}
