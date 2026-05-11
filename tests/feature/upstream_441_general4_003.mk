%.foo : baz$$bar ; @echo 'done $<'
%.foo : bar$$baz ; @echo 'done $<'
test.foo:
baz$$bar bar$$baz: ; @echo '$@'
