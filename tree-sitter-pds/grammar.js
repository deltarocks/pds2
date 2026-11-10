module.exports = grammar({
    name: 'pds',
    file_types: ['pds'],

    conflicts: $ => [
        [$.stringBlock, $.stringSingleline],
        [$.item, $.boolean],
    ],

    rules: {
        source_file: $ => repeat($.statement),

        block: $ => seq(
            $.lbrace,
            repeat($.statement),
            $.rbrace,
        ),

        statement: $ => seq(
            repeat($.attribute),
            $.valueChain,
            ";"
        ),

        attributeName: $ => seq(
            '@',
            field('name', $.type_identifier)
        ),

        attribute: $ => seq(
            $.attributeName,
            ' ',
            $.valueChain,
        ),

        value: $ => choice(
            $.reference,
            $.string,
            $.number,
            $.item,
            $.boolean,
        ),

        referencePart: $ => seq(
            optional('@'),
            $.type_identifier,
        ),
        reference: $ => seq(
            choice(
                './',
                repeat1('../'),
            ),
            repeat(seq(
                $.referencePart,
                '/',
            )),
            $.referencePart,
        ),

        number: $ => /[0-9]+/,

        itemArg: $ => seq(
            optional(seq(
                $.pathIdent,
                ":",
            )),
            $.value,
        ),

        item: $ => seq(
            $.path,
            optional(seq(
                $.lparen,
                repeat(seq($.itemArg, ',')),
                $.itemArg,
                $.rparen,
            )),
            optional($.block),
        ),

        valueChain: $ => seq(
            repeat(
                seq(
                    $.value,
                    $.rocket,
                )
            ),
            $.value,
        ),

        string: $ => choice($.stringSingleline, $.stringBlock),
        boolean: $ => choice(token('true'), token('false')),

        stringSingleline: $ => token(/"([^"\\\n]|\\.)*"/),
        stringBlock: $ => token(seq(
            "\"\"\"\n",
            /[^"]+/,
            "\"\"\"",
        )),

        rootPath: $ => seq(
            $.root,
            $.pathDelim,
        ),
        pathItem: $ => seq(
            $.pathIdent,
            $.pathDelim,
        ),
        path: $ => seq(
            optional($.rootPath),
            repeat($.pathItem),
            $.type_identifier,
        ),

        root: $ => token('root'),
        pathDelim: $ => token('::'),

        lparen: $ => token("("),
        rparen: $ => token(")"),

        lbrace: $ => token("{"),
        rbrace: $ => token("}"),

        rocket: $ => token('=>'),
        special_type: $ => choice(
            'def',
            'container',
            'field',
            'virtual_field',
        ),
        pathIdent: $ => $.type_identifier,
        type_identifier: $ => token(/[a-zA-Z_][a-zA-Z_0-9]*/),
    }
});
