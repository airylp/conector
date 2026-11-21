#!/usr/bin/env python3
"""Cliente de ejemplo del servicio de conector (solo biblioteca estándar, Python 3.8+).

Variables de entorno:
  CONECTOR_URL    https://equipo:7470   (por defecto https://localhost:7470)
  CONECTOR_TOKEN  token del servicio (Ajustes → Servicio de red, o `conector --servir`)
  CONECTOR_CA     certificado del servicio (servicio-cert.pem); sin él se usan
                  los certificados del sistema (para un certificado propio de una CA)

Uso:
  cliente.py lista
  cliente.py probar ID
  cliente.py consulta ID "select * from clientes" [MAX]
  cliente.py eventos            # muestra cada cambio del catálogo al momento
"""

import json
import os
import ssl
import sys
import urllib.request

URL = os.environ.get("CONECTOR_URL", "https://localhost:7470").rstrip("/")
TOKEN = os.environ.get("CONECTOR_TOKEN", "")
CA = os.environ.get("CONECTOR_CA")


def _contexto():
    if not URL.startswith("https"):
        return None
    return ssl.create_default_context(cafile=CA) if CA else ssl.create_default_context()


def _pedir(metodo, ruta, cuerpo=None, timeout=330):
    datos = cuerpo.encode("utf-8") if cuerpo is not None else None
    req = urllib.request.Request(URL + ruta, data=datos, method=metodo, headers={"Authorization": "Bearer " + TOKEN})
    return urllib.request.urlopen(req, context=_contexto(), timeout=timeout)


def _json(metodo, ruta, cuerpo=None):
    try:
        with _pedir(metodo, ruta, cuerpo) as r:
            return json.load(r)
    except urllib.error.HTTPError as e:
        return json.load(e)


def conexiones():
    """Lista de conexiones del catálogo (sin contraseñas)."""
    return _json("GET", "/v1/conexiones")


def probar(id_):
    """{'ok': True, 'info': {...}} o {'ok': False, 'error': {...}}."""
    return _json("POST", f"/v1/conexiones/{id_}/probar")


def consulta(id_, sql, maximo=1000):
    """{'ok': True, 'columnas': [...], 'filas': [[...]], 'mas': bool, ...}."""
    return _json("POST", f"/v1/conexiones/{id_}/consulta?max={int(maximo)}", sql)


def eventos():
    """Generador de (evento, datos) del flujo SSE; bloquea hasta cada cambio."""
    with _pedir("GET", "/v1/eventos", timeout=None) as r:
        evento, datos = None, []
        for linea in r:
            linea = linea.decode("utf-8").rstrip("\r\n")
            if linea.startswith("event:"):
                evento = linea[6:].strip()
            elif linea.startswith("data:"):
                datos.append(linea[5:].strip())
            elif linea == "" and datos:
                yield evento, json.loads("\n".join(datos))
                evento, datos = None, []


def main(args):
    if not args:
        print(__doc__)
        return 2
    orden = args[0]
    if orden == "lista":
        for c in conexiones():
            print(f"{c['id']}\t{c['nombre']}\t{c['url']}")
    elif orden == "probar":
        r = probar(args[1])
        print(json.dumps(r, ensure_ascii=False, indent=2))
        return 0 if r.get("ok") else 1
    elif orden == "consulta":
        r = consulta(args[1], args[2], int(args[3]) if len(args) > 3 else 1000)
        if not r.get("ok"):
            print(json.dumps(r, ensure_ascii=False, indent=2))
            return 1
        if "columnas" in r:
            print("\t".join(c["nombre"] for c in r["columnas"]))
            for fila in r["filas"]:
                print("\t".join("NULL" if v is None else v for v in fila))
        print(f"-- {len(r.get('filas', []))} filas, {r['ms']} ms" + (" (hay más)" if r.get("mas") else ""), file=sys.stderr)
    elif orden == "eventos":
        for evento, datos in eventos():
            print(evento, json.dumps(datos, ensure_ascii=False), flush=True)
    else:
        print(__doc__)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
