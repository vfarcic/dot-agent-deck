import sys
W,H,R=330,260,28      # card size, corner radius
DX,DY=44,46           # stack offset per card
TB=58                 # title bar height
X0,Y0=47,80           # back card origin in a 512 box

def card(x,y,title,body,dot,prompt=None,stroke=None,sw=0,dots=True):
    s=[]
    st=f' stroke="{stroke}" stroke-width="{sw}" paint-order="stroke"' if stroke else ''
    s.append(f'<rect x="{x}" y="{y}" width="{W}" height="{H}" rx="{R}" fill="{title}"{st}/>')
    if body:
        # body: square top corners, rounded bottom corners
        s.append(f'<path d="M{x} {y+TB}h{W}v{H-TB-R}a{R} {R} 0 0 1 -{R} {R}h-{W-2*R}a{R} {R} 0 0 1 -{R} -{R}z" fill="{body}"/>')
    if dots:
        cy = y+(TB/2 if prompt else 24)
        for i in range(3):
            s.append(f'<circle cx="{x+34+i*34}" cy="{cy}" r="11" fill="{dot}"/>')
    if prompt:
        s.append(f'<path d="M{x+66} {y+116}l54 44-54 44" fill="none" stroke="{prompt}" stroke-width="28" stroke-linecap="round" stroke-linejoin="round"/>')
        s.append(f'<path d="M{x+160} {y+204}h84" fill="none" stroke="{prompt}" stroke-width="26" stroke-linecap="round"/>')
    return "\n  ".join(s)

def svg(inner, rot=0, tile=None, rim=None):
    t=''
    if tile:
        t=f'<rect width="512" height="512" rx="112" fill="{tile}"/>\n  '
        if rim: t+=f'<rect x="5" y="5" width="502" height="502" rx="107" fill="none" stroke="{rim}" stroke-width="10"/>\n  '
    g=f'<g transform="rotate({rot} 256 256)">' if rot else '<g>'
    return f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 512 512">\n  {t}{g}\n  {inner}\n  </g>\n</svg>\n'

def colour(dots=True, body="#0e1418", sep=None):
    return "\n  ".join([
        card(X0,Y0,"#6248d6",None,"#ffffff",stroke=sep,sw=14 if sep else 0,dots=dots),
        card(X0+DX,Y0+DY,"#4ecdc4",None,"#ffffff",stroke=sep,sw=14 if sep else 0,dots=dots),
        card(X0+2*DX,Y0+2*DY,"#0a7c73",body,"#ffffff",prompt="#ffffff",stroke=sep,sw=14 if sep else 0,dots=dots),
    ])

def mono(fg="#ffffff", bg="#0e1418", dots=True):
    # scaled a touch smaller to sit inside the tile
    inner="\n  ".join([
        card(X0,Y0,fg,None,bg,stroke=bg,sw=16,dots=dots),
        card(X0+DX,Y0+DY,fg,None,bg,stroke=bg,sw=16,dots=dots),
        card(X0+2*DX,Y0+2*DY,fg,None,bg,prompt=bg,stroke=bg,sw=16,dots=dots),
    ])
    return f'<g transform="translate(256 256) scale(0.8) translate(-256 -256)">{inner}</g>'

out=sys.argv[1]
open(f"{out}/colour.svg","w").write(svg(colour()))
open(f"{out}/colour-tilt.svg","w").write(svg(colour(),rot=-5))
open(f"{out}/colour-small.svg","w").write(svg(colour(dots=False)))
open(f"{out}/mono-tile.svg","w").write(svg(mono(),tile="#0e1418"))
open(f"{out}/mono-tile-small.svg","w").write(svg(mono(dots=False),tile="#0e1418"))

def scaled(inner, k=0.8):
    return f'<g transform="translate(256 256) scale({k}) translate(-256 -256)">{inner}</g>'
# app icon candidates: colour symbol on an ink tile with a faint rim, and on a light tile
open(f"{out}/icon-ink.svg","w").write(svg(scaled(colour()),tile="#0e1418",rim="#2c3a46"))
open(f"{out}/icon-light.svg","w").write(svg(scaled(colour()),tile="#f5f7f9"))
# colour symbol whose front body is lifted off near-black so its edge survives on dark
open(f"{out}/colour-lifted.svg","w").write(svg(colour(body="#18232c")))
# 16px master: two cards, no dots, oversized prompt, no underscore
def fav():
    w,h,r=372,300,40
    s=[f'<rect x="40" y="56" width="{w}" height="{h}" rx="{r}" fill="#6248d6"/>',
       f'<rect x="100" y="156" width="{w}" height="{h}" rx="{r}" fill="#0a7c73"/>',
       f'<path d="M100 236h{w}v{h-80-r}a{r} {r} 0 0 1 -{r} {r}h-{w-2*r}a{r} {r} 0 0 1 -{r} -{r}z" fill="#0e1418"/>',
       '<path d="M178 290l80 64-80 64" fill="none" stroke="#ffffff" stroke-width="52" stroke-linecap="round" stroke-linejoin="round"/>',
       '<path d="M300 408h96" fill="none" stroke="#ffffff" stroke-width="46" stroke-linecap="round"/>']
    return "\n  ".join(s)
open(f"{out}/fav16.svg","w").write(svg(fav()))
open(f"{out}/fav16-ink.svg","w").write(svg(scaled(fav(),0.86),tile="#0e1418",rim="#3a4a58"))
