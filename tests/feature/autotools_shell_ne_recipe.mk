all:
	@srcdirstrip=x; \
	if test $$d != y; then :; fi
	-test -n "$(am__skip_mode_fix)" \
	|| printf 'mode-fix\n'
