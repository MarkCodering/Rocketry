import json, sys
for line in sys.stdin:
    request = json.loads(line)
    if 'id' not in request: continue
    method = request['method']
    if method == 'initialize':
        result = {'protocolVersion': request['params']['protocolVersion'], 'capabilities': {'tools': {}}, 'serverInfo': {'name': 'fixture', 'version': '1'}}
    elif method == 'tools/list':
        result = {'tools': [{'name': 'echo', 'description': 'Echo input', 'inputSchema': {'type': 'object', 'properties': {'text': {'type': 'string'}}, 'required': ['text']}}]}
    elif method == 'tools/call':
        result = {'content': [{'type': 'text', 'text': request['params']['arguments']['text']}]}
    else:
        result = {}
    print(json.dumps({'jsonrpc': '2.0', 'id': request['id'], 'result': result}), flush=True)
