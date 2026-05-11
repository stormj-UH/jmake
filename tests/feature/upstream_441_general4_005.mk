all: dir/subdir/file.$$a

dir/subdir: ; @echo mkdir -p '$@'

dir/subdir/file.$$b: dir/subdir ; @echo touch '$@'

dir/subdir/%.$$a: dir/subdir/%.$$b ; @echo 'cp $< $@'
