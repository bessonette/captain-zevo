# Captain Zevo plugins

Captain Zevo reads plugins from this folder. A plugin is a TOML file:
`<name>.toml` here, or `<name>/plugin.toml` when it comes with scripts of its
own. Programs called `zevo-<name>` in a `bin` folder here become `zevo <name>`.

A plugin can add starter templates, extras and editors, and can name programs
Zevo runs: hooks after a change, checks for `zevo doctor`, and a certificate
issuer.

    zevo plugins new my-plugin   # write a commented starting point here
    zevo plugins                 # what is loaded, and anything that was skipped
    zevo plugins --builtin       # what Zevo ships with, in the same format

The full format, and examples to copy: {docs}

Programs named by a plugin run with your permissions. Only install plugins
you trust.

Zevo wrote this file and will not change or replace it; delete it if you
like.
