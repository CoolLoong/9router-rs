const fs = require("node:fs");
const crypto = require("node:crypto");
const { SignedXml } = require("xml-crypto");

const [keyFile, certificateFile, requestId, endpoint, audience, mode] = process.argv.slice(2);
const certificate = fs.readFileSync(certificateFile, "utf8");
const signingKey = fs.readFileSync(keyFile, "utf8");
const started = new Date(Date.now() - 5000).toISOString();
const expires = new Date(Date.now() + 300000).toISOString();
const assertion = "_" + crypto.randomBytes(20).toString("hex");
const response = "_" + crypto.randomBytes(20).toString("hex");
const encode = value => value.replaceAll("&", "&amp;").replaceAll("<", "&lt;").replaceAll('"', "&quot;");
const issuer = "urn:fixture:idp";
const authn = '<saml:AuthnContext><saml:AuthnContextClassRef>urn:oasis:names:tc:SAML:2.0:ac:classes:PasswordProtectedTransport</saml:AuthnContextClassRef></saml:AuthnContext>';
const xml = `<samlp:Response xmlns:samlp="urn:oasis:names:tc:SAML:2.0:protocol" xmlns:saml="urn:oasis:names:tc:SAML:2.0:assertion" ID="${response}" Version="2.0" IssueInstant="${started}" Destination="${encode(endpoint)}" InResponseTo="${encode(requestId)}"><saml:Issuer>${issuer}</saml:Issuer><samlp:Status><samlp:StatusCode Value="urn:oasis:names:tc:SAML:2.0:status:Success"/></samlp:Status><saml:Assertion ID="${assertion}" Version="2.0" IssueInstant="${started}"><saml:Issuer>${issuer}</saml:Issuer><saml:Subject><saml:NameID Format="urn:oasis:names:tc:SAML:1.1:nameid-format:emailAddress">fixture@example.invalid</saml:NameID><saml:SubjectConfirmation Method="urn:oasis:names:tc:SAML:2.0:cm:bearer"><saml:SubjectConfirmationData Recipient="${encode(endpoint)}" InResponseTo="${encode(requestId)}" NotOnOrAfter="${expires}"/></saml:SubjectConfirmation></saml:Subject><saml:Conditions NotBefore="${started}" NotOnOrAfter="${expires}"><saml:AudienceRestriction><saml:Audience>${encode(audience)}</saml:Audience></saml:AudienceRestriction></saml:Conditions><saml:AuthnStatement AuthnInstant="${started}" SessionIndex="fixture-session">${authn}</saml:AuthnStatement><saml:AttributeStatement><saml:Attribute Name="fixtureName"><saml:AttributeValue>Fixture SAML User</saml:AttributeValue></saml:Attribute><saml:Attribute Name="fixtureMail"><saml:AttributeValue>fixture+sso@example.invalid</saml:AttributeValue></saml:Attribute></saml:AttributeStatement></saml:Assertion></samlp:Response>`;
function signed(input, id) {
  const canonicalizationAlgorithm = "http://www.w3.org/2001/10/xml-exc-c14n#";
  const sig = new SignedXml({
    privateKey: signingKey,
    publicCert: certificate,
    canonicalizationAlgorithm,
    signatureAlgorithm: "http://www.w3.org/2001/04/xmldsig-more#rsa-sha256",
  });
  sig.addReference({
    xpath: `//*[@ID='${id}']`,
    transforms: ["http://www.w3.org/2000/09/xmldsig#enveloped-signature", canonicalizationAlgorithm],
    digestAlgorithm: "http://www.w3.org/2001/04/xmlenc#sha256",
  });
  sig.computeSignature(input, {
    prefix: "ds",
    location: { reference: `//*[@ID='${id}']/*[local-name(.)='Issuer']`, action: "after" },
  });
  return sig.getSignedXml();
}
let result = signed(xml, assertion);
if (mode !== "assertion-only") result = signed(result, response);
process.stdout.write(Buffer.from(result).toString("base64"));
