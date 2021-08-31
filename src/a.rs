// File: "mod.rs"
#![allow(non_camel_case_types, dead_code, unused_imports)]
use crate::pds::{Input, Output, Result};
// root::arrayreftest_field_arr
struct arrayreftest_field_arr_args<'a> {
    size: &'a u8,
}
type arrayreftest_field_arr = Vec<u32>;
fn arrayreftest_field_arr_serialize(
    output: &mut Output,
    value: arrayreftest_field_arr,
) -> Result<()> {
    for item in value {
        u32_serialize(output, item)?;
    }
    Ok(())
}
fn arrayreftest_field_arr_deserialize(
    input: &mut Input,
    args: arrayreftest_field_arr_args<'_>,
) -> Result<arrayreftest_field_arr> {
    let mut out = Vec::with_capacity(*args.size as usize);
    for _ in 0..(*args.size as usize) {
        out.push(u32_deserialize(input)?);
    }
    Ok(out)
}
fn arrayreftest_field_arr_size_of(value: &arrayreftest_field_arr) -> usize {
    value.iter().map(|v| u32_size_of(&v)).sum()
}
// root::arrayreftest_field
struct arrayreftest_field_args<'a> {
    size: &'a u8,
}
struct arrayreftest_field {
    arr: arrayreftest_field_arr,
}
fn arrayreftest_field_serialize(output: &mut Output, value: arrayreftest_field) -> Result<()> {
    arrayreftest_field_arr_serialize(output, value.arr)?;
    Ok(())
}
fn arrayreftest_field_deserialize(
    input: &mut Input,
    args: arrayreftest_field_args<'_>,
) -> Result<arrayreftest_field> {
    let arr = arrayreftest_field_arr_deserialize(
        input,
        arrayreftest_field_arr_args { size: &args.size },
    )?;
    Ok(arrayreftest_field { arr })
}
fn arrayreftest_field_size_of(value: &arrayreftest_field) -> usize {
    arrayreftest_field_arr_size_of(&value.arr)
}
// root::arrayreftest
struct arrayreftest {
    // size - virtual field
    field: arrayreftest_field,
}
fn arrayreftest_serialize(output: &mut Output, value: arrayreftest) -> Result<()> {
    u8_serialize(
        output,
        value /*TypeId(Invalid)*/
            .field /*TypeId(Invalid)*/
            .arr /*TypeId(Invalid)*/
            .len() as u8,
    )?;
    arrayreftest_field_serialize(output, value.field)?;
    Ok(())
}
fn arrayreftest_deserialize(input: &mut Input) -> Result<arrayreftest> {
    let size = u8_deserialize(input)?;
    let field = arrayreftest_field_deserialize(input, arrayreftest_field_args { size: &size })?;
    Ok(arrayreftest { field })
}
fn arrayreftest_size_of(value: &arrayreftest) -> usize {
    u8_size_of(
        &value /*TypeId(Invalid)*/
            .field /*TypeId(Invalid)*/
            .arr /*TypeId(Invalid)*/
            .len(),
    ) + arrayreftest_field_size_of(&value.field)
}
use crate::pds::{u32, u32_deserialize, u32_serialize, u32_size_of};
use crate::pds::{u64, u64_deserialize, u64_serialize, u64_size_of};
use crate::pds::{u8, u8_deserialize, u8_serialize, u8_size_of};
