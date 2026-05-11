bar = Good$$bye
foo :::= $(bar) $$what
bar = ${ugh}
ugh = Hello
all: ; @echo '$(foo)'
