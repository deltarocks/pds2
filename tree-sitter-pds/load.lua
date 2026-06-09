local parser_config = require "nvim-treesitter.parsers".get_parser_configs()
vim.opt.runtimepath:append("~/build/pds2/tree-sitter-pds")
parser_config.pds = {
	install_info = {
		url = "~/build/pds2/tree-sitter-pds",
		files = {'src/parser.c'},

		requires_generate_from_grammar = false,
	},
	filetype = "pds",
}
vim.cmd [[TSInstall pds]]
vim.filetype.add {
	extension = {
		pds = 'pds',
	},
}
