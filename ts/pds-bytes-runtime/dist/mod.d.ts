export type ErrorKind = "NotEnoughData" | "TooLargeInteger" | "InvalidValue";
export declare class PdsError extends Error {
    readonly kind: ErrorKind;
    constructor(kind: ErrorKind, detail?: string);
}
export declare class Input {
    #private;
    constructor(data: Uint8Array);
    get offset(): number;
    remaining(): number;
    mask_next(and: number): void;
    peek(): number;
    read(): number;
    skip(): void;
    rewind(offset: number): void;
    read_bytes(n: number): Uint8Array<ArrayBuffer>;
    read_rest(): Uint8Array<ArrayBuffer>;
    take(n: number): Input;
}
export declare class Output {
    #private;
    get length(): number;
    write(bytes: ArrayLike<number>): void;
    write_byte(byte: number): void;
    into_inner(): Uint8Array<ArrayBuffer>;
}
export declare function equal(a: unknown, b: unknown): boolean;
export declare function utf8_encode(s: string): Uint8Array;
export declare function utf8_decode(bytes: Uint8Array): string;
export interface Native<T> {
    serialize(v: T, output: Output): void;
    deserialize(input: Input): T;
    size_of(v: T): number;
}
export declare const pds_u8: Native<number>;
export declare const pds_i8: Native<number>;
export declare const pds_u16: Native<number>;
export declare const pds_i16: Native<number>;
export declare const pds_u32: Native<number>;
export declare const pds_i32: Native<number>;
export declare const pds_u64: Native<bigint>;
export declare const pds_i64: Native<bigint>;
export declare const pds_f32: Native<number>;
export declare const pds_f64: Native<number>;
export declare const pds_bool: Native<boolean>;
export declare const pds_void: Native<null>;
