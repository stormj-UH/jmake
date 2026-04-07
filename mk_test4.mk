.SECONDEXPANSION:
$(dir)/tmp/bar.o:

$(dir)/tmp/foo/bar.c: ; @echo '$@'
$(dir)/tmp/bar/bar.c: ; @echo '$@'
foo.h: ; @echo '$@'

%.o: $$(addsuffix /%.c,foo bar) foo.h ; @echo '$@: {$<} $^'
