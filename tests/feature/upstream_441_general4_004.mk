%.foo : %$$bar ; @echo 'done $<'
test.foo:
test$$bar: ; @echo '$@'
