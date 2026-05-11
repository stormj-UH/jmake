bar = Good$$bye
foo :::= $(bar)
foo += $$what $(bar)
bar = ${ugh}
ugh = Hello
all: ; @echo '$(foo)'
