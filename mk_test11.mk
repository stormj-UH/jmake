.SECONDEXPANSION:
PREREQS=p1|p2
P2=p2
all : foo bar
f%o: $$(PREREQS) ; @echo '$@' from '$^' and '$|'
b%r: p1|$$(P2)   ; @echo '$@' from '$^' and '$|'
p% : ; : $@
