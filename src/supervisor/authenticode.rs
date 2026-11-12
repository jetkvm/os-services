//! The publisher check for an update on Windows. An update is an installer
//! that runs as the desktop user, so the service runs only an installer
//! that its own publisher signed.
//!
//! The service's own executable names the publisher. Its Authenticode
//! signature holds the certificates up to the root, and that root is the
//! only trust anchor of the check. The check does not trust the roots of
//! the Windows root store and fetches nothing from the network, so a
//! computer that was never online, where the publisher's root is not
//! installed, trusts the same root as any other. Windows can still take a
//! certificate between the signer and the root from its own stores when the
//! file does not hold it.
//!
//! A signing certificate can be valid for a few days: Azure Artifact
//! Signing issues a new one each day, valid for 72 hours. Two things name
//! the publisher across certificates: the identity that Azure puts in the
//! extended key usage of each certificate of one certificate profile, and,
//! for a certificate without one, the subject.
//!
//! A certificate that has expired still proves a signature that was made
//! while it was valid, and the timestamp in the signature gives that time.
//! Windows takes the time from any timestamp, also from one that the
//! attacker's own time server made, so the timestamp's signer must lead to
//! the service's root too.

use std::{
    ffi::CStr,
    fs::File,
    io, mem,
    os::windows::{ffi::OsStrExt, io::AsRawHandle},
    path::Path,
    ptr, slice,
    sync::{Mutex, PoisonError},
};
use winapi::{
    ctypes::c_void,
    shared::minwindef::{BOOL, DWORD, FILETIME, WORD},
    um::{
        handleapi::INVALID_HANDLE_VALUE,
        softpub::WINTRUST_ACTION_GENERIC_VERIFY_V2,
        wincrypt::{
            CertAddCertificateContextToStore, CertAddStoreToCollection, CertCloseStore,
            CertCompareCertificateName, CertCreateCertificateChainEngine,
            CertDuplicateCertificateContext, CertEnumCertificatesInStore, CertFreeCertificateChain,
            CertFreeCertificateChainEngine, CertFreeCertificateContext, CertGetCertificateChain,
            CertGetEnhancedKeyUsage, CertOpenStore, CryptMsgClose, CryptMsgGetParam,
            CryptQueryObject, CERT_CHAIN_CACHE_ONLY_URL_RETRIEVAL, CERT_CHAIN_ENGINE_CONFIG,
            CERT_CHAIN_PARA, CERT_ENHKEY_USAGE, CERT_FIND_EXT_ONLY_ENHKEY_USAGE_FLAG,
            CERT_QUERY_CONTENT_FLAG_PKCS7_SIGNED, CERT_QUERY_CONTENT_FLAG_PKCS7_SIGNED_EMBED,
            CERT_QUERY_FORMAT_FLAG_BINARY, CERT_QUERY_OBJECT_BLOB, CERT_QUERY_OBJECT_FILE,
            CERT_STORE_ADD_ALWAYS, CERT_STORE_PROV_COLLECTION, CERT_STORE_PROV_MEMORY,
            CERT_TRUST_IS_NOT_TIME_VALID, CMSG_SIGNER_UNAUTH_ATTR_PARAM, CRYPT_ATTRIBUTES,
            HCERTCHAINENGINE, HCERTSTORE, HCRYPTMSG, PCCERT_CHAIN_CONTEXT, PCCERT_CONTEXT,
            USAGE_MATCH_TYPE_AND, X509_ASN_ENCODING,
        },
        winnt::HANDLE,
        wintrust::{
            WinVerifyTrust, WINTRUST_DATA, WINTRUST_FILE_INFO, WINTRUST_SIGNATURE_SETTINGS,
            WTD_CACHE_ONLY_URL_RETRIEVAL, WTD_CHOICE_FILE, WTD_REVOCATION_CHECK_NONE,
            WTD_REVOKE_NONE, WTD_STATEACTION_CLOSE, WTD_STATEACTION_VERIFY, WTD_UI_NONE,
        },
    },
};

/// The extended key usages that a chain is built for.
const CODE_SIGNING: &CStr = c"1.3.6.1.5.5.7.3.3";
const TIME_STAMPING: &CStr = c"1.3.6.1.5.5.7.3.8";

/// The unauthenticated attribute that holds an RFC 3161 timestamp, which is
/// a signed message of its own with its own certificates.
const RFC3161_TIMESTAMP: &CStr = c"1.3.6.1.4.1.311.3.3.1";

/// Azure Artifact Signing gives each certificate profile an identity that
/// stays the same from certificate to certificate, and a profile that is
/// deleted and made again gets a new one. A public trust certificate holds
/// the identity as an extended key usage directly under the first arc. The
/// usages under the second arc are markers and the identities of private
/// trust, which this check does not read: a certificate with only those is
/// compared by its subject.
const AZURE_ARC: &str = "1.3.6.1.4.1.311.97.";
const AZURE_KIND_ARC: &str = "1.3.6.1.4.1.311.97.1.";

/// The last step of WinVerifyTrust that concerns the file and its
/// signature: TRUSTERROR_STEP_FINAL_SIGPROV in wintrust.h. The steps after
/// it judge the certificates against the Windows root store.
const LAST_SIGNATURE_STEP: usize = 33;

/// Every kind of signature block: CERT_SECTION_TYPE_ANY in wintrust.h.
const ANY_SIGNATURE_BLOCK: WORD = 0xFF;

/// WSS_VERIFY_SPECIFIC in wintrust.h: WinVerifyTrust checks the signature
/// with the given index and no other.
const VERIFY_ONE_SIGNATURE: DWORD = 0x1;

/// The leading fields of CRYPT_PROVIDER_DATA, CRYPT_PROVIDER_SGNR and
/// CRYPT_PROVIDER_CERT in wintrust.h, which winapi does not declare.
/// Windows allocates these structures, and this module reads them through
/// the pointers that the three functions below return.
#[repr(C)]
#[allow(non_snake_case)]
struct ProviderData {
    cbStruct: DWORD,
    pWintrustData: *mut c_void,
    fOpenedFile: BOOL,
    hWndParent: *mut c_void,
    pgActionID: *mut c_void,
    hProv: usize,
    dwError: DWORD,
    dwRegSecuritySettings: DWORD,
    dwRegPolicySettings: DWORD,
    psPfns: *mut c_void,
    cdwTrustStepErrors: DWORD,
    padwTrustStepErrors: *mut DWORD,
}

#[repr(C)]
#[allow(non_snake_case)]
struct ProviderSigner {
    cbStruct: DWORD,
    sftVerifyAsOf: FILETIME,
    csCertChain: DWORD,
    pasCertChain: *mut ProviderCertificate,
    dwSignerType: DWORD,
    psSigner: *mut c_void,
    dwError: DWORD,
    csCounterSigners: DWORD,
}

#[repr(C)]
#[allow(non_snake_case)]
struct ProviderCertificate {
    cbStruct: DWORD,
    pCert: PCCERT_CONTEXT,
}

#[link(name = "wintrust")]
extern "system" {
    fn WTHelperProvDataFromStateData(state: HANDLE) -> *mut ProviderData;
    fn WTHelperGetProvSignerFromChain(
        data: *mut ProviderData,
        signer: DWORD,
        counter_signer: BOOL,
        counter_signer_index: DWORD,
    ) -> *mut ProviderSigner;
    fn WTHelperGetProvCertFromChain(
        signer: *mut ProviderSigner,
        certificate: DWORD,
    ) -> *mut ProviderCertificate;
}

#[link(name = "imagehlp")]
extern "system" {
    fn ImageEnumerateCertificates(
        file: HANDLE,
        kind: WORD,
        count: *mut DWORD,
        indices: *mut DWORD,
        index_count: DWORD,
    ) -> BOOL;
}

/// The functions of imagehlp are for one thread at a time.
static IMAGEHLP: Mutex<()> = Mutex::new(());

/// What the check found for a package that it does not refuse.
#[derive(Debug, PartialEq, Eq)]
pub enum Publisher {
    /// The program has no signature, so it names no publisher, and the
    /// package was not checked.
    Unknown,
    /// The publisher of the program signed the package.
    Same,
}

/// Compares the publisher of `package` with the publisher of `program`.
/// The error is a sentence that says why the package is refused.
pub fn check(program: &Path, package: &Path) -> Result<Publisher, String> {
    let own = |reason| format!("this service cannot name its publisher: {reason}");
    let refused = |reason| format!("the package is refused: {reason}");
    let Some(program) = Signature::read(program).map_err(own)? else {
        return Ok(Publisher::Unknown);
    };
    let root = program.root().map_err(own)?;
    let package = Signature::read(package)
        .map_err(refused)?
        .ok_or_else(|| refused("it has no signature".into()))?;
    package.leads_to(&root).map_err(refused)?;
    if package.signer.identity() != program.signer.identity() {
        return Err(refused("another publisher signed it".into()));
    }
    Ok(Publisher::Same)
}

struct Store(HCERTSTORE);

impl Store {
    /// A store of the given kind that holds nothing.
    fn empty(kind: *const i8) -> Result<Self, String> {
        let store = unsafe { CertOpenStore(kind, 0, 0, 0, ptr::null()) };
        if store.is_null() {
            return Err(windows_error("a certificate store could not be made"));
        }
        Ok(Self(store))
    }

    /// A store in memory that holds copies of these certificates.
    fn of(certificates: &[&Certificate]) -> Result<Self, String> {
        let store = Self::empty(CERT_STORE_PROV_MEMORY)?;
        for certificate in certificates {
            let added = unsafe {
                CertAddCertificateContextToStore(
                    store.0,
                    certificate.0,
                    CERT_STORE_ADD_ALWAYS,
                    ptr::null_mut(),
                )
            };
            if added == 0 {
                return Err(windows_error("a certificate could not be copied"));
            }
        }
        Ok(store)
    }

    fn certificates(&self) -> Vec<Certificate> {
        let mut certificates = Vec::new();
        let mut certificate = ptr::null();
        loop {
            // The call frees the certificate it is given and returns the
            // next one, so each one that is kept is duplicated.
            certificate = unsafe { CertEnumCertificatesInStore(self.0, certificate) };
            if certificate.is_null() {
                return certificates;
            }
            certificates.push(unsafe { Certificate::duplicate(certificate) });
        }
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        unsafe { CertCloseStore(self.0, 0) };
    }
}

struct Certificate(PCCERT_CONTEXT);

/// Who a certificate names as the publisher.
#[derive(Debug, PartialEq, Eq)]
enum Identity {
    /// The Azure identities in the extended key usage, sorted.
    Azure(Vec<String>),
    /// The encoded subject.
    Subject(Vec<u8>),
}

impl Certificate {
    /// A reference of its own to a certificate that Windows owns.
    unsafe fn duplicate(certificate: PCCERT_CONTEXT) -> Self {
        Self(CertDuplicateCertificateContext(certificate))
    }

    fn encoded(&self) -> &[u8] {
        unsafe { items((*self.0).pbCertEncoded, (*self.0).cbCertEncoded) }
    }

    /// Whether the certificate names itself as its issuer, as a root does.
    fn self_issued(&self) -> bool {
        unsafe {
            let info = (*self.0).pCertInfo;
            CertCompareCertificateName(
                X509_ASN_ENCODING,
                ptr::addr_of_mut!((*info).Subject),
                ptr::addr_of_mut!((*info).Issuer),
            ) != 0
        }
    }

    fn identity(&self) -> Identity {
        let mut azure: Vec<String> = self
            .usages()
            .into_iter()
            .filter(|usage| usage.starts_with(AZURE_ARC) && !usage.starts_with(AZURE_KIND_ARC))
            .collect();
        if !azure.is_empty() {
            azure.sort();
            return Identity::Azure(azure);
        }
        let subject = unsafe { &(*(*self.0).pCertInfo).Subject };
        Identity::Subject(unsafe { items(subject.pbData, subject.cbData) }.to_vec())
    }

    /// The extended key usages in the certificate itself; none when it has
    /// no such extension.
    fn usages(&self) -> Vec<String> {
        unsafe {
            let mut size = 0;
            let own = CERT_FIND_EXT_ONLY_ENHKEY_USAGE_FLAG;
            if CertGetEnhancedKeyUsage(self.0, own, ptr::null_mut(), &mut size) == 0 || size == 0 {
                return Vec::new();
            }
            // The answer is a structure followed by the data its pointers
            // refer to, so the buffer is aligned for pointers.
            let mut buffer = vec![0u64; size as usize / 8 + 1];
            let usage = buffer.as_mut_ptr().cast::<CERT_ENHKEY_USAGE>();
            if CertGetEnhancedKeyUsage(self.0, own, usage, &mut size) == 0 {
                return Vec::new();
            }
            items((*usage).rgpszUsageIdentifier, (*usage).cUsageIdentifier)
                .iter()
                .map(|identifier| CStr::from_ptr(*identifier).to_string_lossy().into_owned())
                .collect()
        }
    }
}

impl Drop for Certificate {
    fn drop(&mut self) {
        unsafe { CertFreeCertificateContext(self.0) };
    }
}

/// The signature in a file.
struct Signature {
    signer: Certificate,
    /// The signers of the signature's timestamps.
    time_servers: Vec<Certificate>,
    /// The time the signature is judged at: the time in its timestamp, or
    /// the present for a signature without one.
    made: FILETIME,
    certificates: Certificates,
}

impl Signature {
    /// Reads the first signature of a file and checks it against the
    /// file's content. None when the file has no signature. A signature
    /// that is there and cannot be read is an error.
    fn read(file: &Path) -> Result<Option<Self>, String> {
        if signature_blocks(file)? == 0 {
            return Ok(None);
        }
        let path: Vec<u16> = file.as_os_str().encode_wide().chain(Some(0)).collect();
        let certificates = unsafe { Certificates::in_file(&path) }?;
        let verified = unsafe { verify(&path) }?;
        Ok(Some(Self {
            signer: verified.signer,
            time_servers: verified.time_servers,
            made: verified.made,
            certificates,
        }))
    }

    /// The root that the signer leads to through the certificates in the
    /// file. The file is the program that runs, so its signature is not
    /// judged by time: a certificate that has expired still names the
    /// publisher.
    fn root(&self) -> Result<Certificate, String> {
        let roots: Vec<Certificate> = self
            .certificates
            .all
            .certificates()
            .into_iter()
            .filter(Certificate::self_issued)
            .collect();
        if roots.is_empty() {
            return Err("its signature holds no root certificate".into());
        }
        let anchors = Store::of(&roots.iter().collect::<Vec<_>>())?;
        self.chain(
            &anchors,
            &self.signer,
            CODE_SIGNING,
            CERT_TRUST_IS_NOT_TIME_VALID,
        )
        .map_err(|reason| format!("its signature does not lead to a root certificate ({reason})"))
    }

    /// Fails unless the signer and every time server lead to `root` at the
    /// time the signature was made.
    fn leads_to(&self, root: &Certificate) -> Result<(), String> {
        let anchors = Store::of(&[root])?;
        let reached = self
            .chain(&anchors, &self.signer, CODE_SIGNING, 0)
            .map_err(|reason| {
                format!("its signature does not lead to the root certificate of this service ({reason})")
            })?;
        if reached.encoded() != root.encoded() {
            return Err("its signature leads to another root certificate".into());
        }
        for time_server in &self.time_servers {
            self.chain(&anchors, time_server, TIME_STAMPING, 0)
                .map_err(|reason| {
                    format!("its timestamp does not lead to the root certificate of this service ({reason})")
                })?;
        }
        Ok(())
    }

    /// Builds the chain of `certificate` for `usage` at the time the
    /// signature was made, and returns the root it ends at. Only `anchors`
    /// are trusted, and the certificates between come from the file.
    /// `tolerated` holds the chain errors that do not count.
    fn chain(
        &self,
        anchors: &Store,
        certificate: &Certificate,
        usage: &CStr,
        tolerated: DWORD,
    ) -> Result<Certificate, String> {
        unsafe {
            let mut config: CERT_CHAIN_ENGINE_CONFIG = mem::zeroed();
            config.cbSize = mem::size_of::<CERT_CHAIN_ENGINE_CONFIG>() as DWORD;
            // With an exclusive root store, the engine trusts no root of
            // the Windows root store.
            config.hExclusiveRoot = anchors.0;
            let mut engine = ptr::null_mut();
            if CertCreateCertificateChainEngine(&mut config, &mut engine) == 0 {
                return Err(windows_error("the chain engine could not be made"));
            }
            let engine = Engine(engine);
            let mut usage = usage.as_ptr().cast_mut();
            let mut parameters: CERT_CHAIN_PARA = mem::zeroed();
            parameters.cbSize = mem::size_of::<CERT_CHAIN_PARA>() as DWORD;
            parameters.RequestedUsage.dwType = USAGE_MATCH_TYPE_AND;
            parameters.RequestedUsage.Usage.cUsageIdentifier = 1;
            parameters.RequestedUsage.Usage.rgpszUsageIdentifier = &mut usage;
            let mut made = self.made;
            let mut chain = ptr::null();
            // No revocation flag, so the engine looks up no revocation, and
            // it reads only its cache, so it fetches nothing.
            let built = CertGetCertificateChain(
                engine.0,
                certificate.0,
                &mut made,
                self.certificates.all.0,
                &mut parameters,
                CERT_CHAIN_CACHE_ONLY_URL_RETRIEVAL,
                ptr::null_mut(),
                &mut chain,
            );
            if built == 0 || chain.is_null() {
                return Err(windows_error("the chain could not be built"));
            }
            let chain = Chain(chain);
            let errors = (*chain.0).TrustStatus.dwErrorStatus & !tolerated;
            if errors != 0 {
                return Err(chain_errors(errors));
            }
            let paths = items((*chain.0).rgpChain, (*chain.0).cChain);
            let last = paths
                .first()
                .and_then(|path| items((**path).rgpElement, (**path).cElement).last())
                .ok_or("the chain is empty")?;
            Ok(Certificate::duplicate((**last).pCertContext))
        }
    }
}

/// The number of signature blocks in a program file. Windows counts them
/// from the header of the program, so "no signature" is an answer of its
/// own, and not one of the failures to read a signature.
fn signature_blocks(file: &Path) -> Result<DWORD, String> {
    let opened = File::open(file).map_err(|error| format!("it could not be opened: {error}"))?;
    let _one_thread = IMAGEHLP.lock().unwrap_or_else(PoisonError::into_inner);
    let mut count = 0;
    let counted = unsafe {
        ImageEnumerateCertificates(
            opened.as_raw_handle().cast(),
            ANY_SIGNATURE_BLOCK,
            &mut count,
            ptr::null_mut(),
            0,
        )
    };
    if counted == 0 {
        return Err(windows_error("it is no program that Windows can read"));
    }
    Ok(count)
}

struct Engine(HCERTCHAINENGINE);

impl Drop for Engine {
    fn drop(&mut self) {
        unsafe { CertFreeCertificateChainEngine(self.0) };
    }
}

struct Chain(PCCERT_CHAIN_CONTEXT);

impl Drop for Chain {
    fn drop(&mut self) {
        unsafe { CertFreeCertificateChain(self.0) };
    }
}

/// The errors of a chain in words, for the log and the KVM.
fn chain_errors(errors: DWORD) -> String {
    const NAMES: [(DWORD, &str); 5] = [
        (
            0x1,
            "a certificate was not valid at the time of the signature",
        ),
        (0x8, "a certificate has a wrong signature"),
        (0x10, "a certificate is not for this use"),
        (0x20, "the root is another one"),
        (0x1_0000, "a certificate is missing"),
    ];
    let mut names: Vec<String> = NAMES
        .iter()
        .filter(|(bit, _)| errors & bit != 0)
        .map(|(_, name)| (*name).to_owned())
        .collect();
    let unnamed = NAMES.iter().fold(errors, |rest, (bit, _)| rest & !bit);
    if unnamed != 0 {
        names.push(format!("chain error 0x{unnamed:X}"));
    }
    names.join(", ")
}

/// The certificates that travel in a file: those of the signature and those
/// of its timestamps.
struct Certificates {
    /// A collection of the stores in `parts`. It closes before they do.
    all: Store,
    _parts: Vec<Store>,
}

impl Certificates {
    /// The certificates of the first signature of a file that has one.
    unsafe fn in_file(path: &[u16]) -> Result<Self, String> {
        let mut store = ptr::null_mut();
        let mut message = ptr::null_mut();
        let found = CryptQueryObject(
            CERT_QUERY_OBJECT_FILE,
            path.as_ptr().cast(),
            CERT_QUERY_CONTENT_FLAG_PKCS7_SIGNED_EMBED,
            CERT_QUERY_FORMAT_FLAG_BINARY,
            0,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            &mut store,
            &mut message,
            ptr::null_mut(),
        );
        if found == 0 {
            return Err(windows_error("its signature could not be read"));
        }
        let message = Message(message);
        let mut parts = vec![Store(store)];
        parts.extend(timestamp_stores(message.0));
        let all = Store::empty(CERT_STORE_PROV_COLLECTION)?;
        for part in &parts {
            if CertAddStoreToCollection(all.0, part.0, 0, 0) == 0 {
                return Err(windows_error("the certificates could not be collected"));
            }
        }
        Ok(Self { all, _parts: parts })
    }
}

struct Message(HCRYPTMSG);

impl Drop for Message {
    fn drop(&mut self) {
        unsafe { CryptMsgClose(self.0) };
    }
}

/// The certificate stores of the RFC 3161 timestamps of a signature.
unsafe fn timestamp_stores(message: HCRYPTMSG) -> Vec<Store> {
    let mut stores = Vec::new();
    let mut size = 0;
    let parameter = CMSG_SIGNER_UNAUTH_ATTR_PARAM;
    if CryptMsgGetParam(message, parameter, 0, ptr::null_mut(), &mut size) == 0 {
        return stores;
    }
    // As for the key usages, the answer holds pointers into itself.
    let mut buffer = vec![0u64; size as usize / 8 + 1];
    if CryptMsgGetParam(message, parameter, 0, buffer.as_mut_ptr().cast(), &mut size) == 0 {
        return stores;
    }
    let attributes = &*buffer.as_ptr().cast::<CRYPT_ATTRIBUTES>();
    for attribute in items(attributes.rgAttr, attributes.cAttr) {
        if CStr::from_ptr(attribute.pszObjId) != RFC3161_TIMESTAMP {
            continue;
        }
        for value in 0..attribute.cValue as usize {
            let mut store = ptr::null_mut();
            let found = CryptQueryObject(
                CERT_QUERY_OBJECT_BLOB,
                attribute.rgValue.add(value).cast(),
                CERT_QUERY_CONTENT_FLAG_PKCS7_SIGNED,
                CERT_QUERY_FORMAT_FLAG_BINARY,
                0,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                &mut store,
                ptr::null_mut(),
                ptr::null_mut(),
            );
            if found != 0 {
                stores.push(Store(store));
            }
        }
    }
    stores
}

/// What WinVerifyTrust read from a signature that fits its file.
struct Verified {
    signer: Certificate,
    time_servers: Vec<Certificate>,
    made: FILETIME,
}

/// Lets Windows check the hash of the file and the signature over it.
unsafe fn verify(path: &[u16]) -> Result<Verified, String> {
    let mut file: WINTRUST_FILE_INFO = mem::zeroed();
    file.cbStruct = mem::size_of::<WINTRUST_FILE_INFO>() as DWORD;
    file.pcwszFilePath = path.as_ptr();
    let mut data: WINTRUST_DATA = mem::zeroed();
    data.cbStruct = mem::size_of::<WINTRUST_DATA>() as DWORD;
    data.dwUIChoice = WTD_UI_NONE;
    data.fdwRevocationChecks = WTD_REVOKE_NONE;
    data.dwUnionChoice = WTD_CHOICE_FILE;
    *data.u.pFile_mut() = &mut file;
    data.dwStateAction = WTD_STATEACTION_VERIFY;
    data.dwProvFlags = WTD_CACHE_ONLY_URL_RETRIEVAL | WTD_REVOCATION_CHECK_NONE;
    // A file can hold more signatures inside its first one. The first one
    // is the signature whose certificates `Certificates::in_file` reads,
    // so Windows checks that one.
    let mut signature: WINTRUST_SIGNATURE_SETTINGS = mem::zeroed();
    signature.cbStruct = mem::size_of::<WINTRUST_SIGNATURE_SETTINGS>() as DWORD;
    signature.dwFlags = VERIFY_ONE_SIGNATURE;
    signature.dwIndex = 0;
    data.pSignatureSettings = &mut signature;
    let mut action = WINTRUST_ACTION_GENERIC_VERIFY_V2;
    let data: *mut WINTRUST_DATA = &mut data;
    // The verdict says whether the Windows root store trusts the signer,
    // which this check does not ask. The steps that Windows went through
    // say whether the signature fits the file.
    WinVerifyTrust(INVALID_HANDLE_VALUE.cast(), &mut action, data.cast());
    let verified = Verified::from_state((*data).hWVTStateData);
    (*data).dwStateAction = WTD_STATEACTION_CLOSE;
    WinVerifyTrust(INVALID_HANDLE_VALUE.cast(), &mut action, data.cast());
    verified
}

impl Verified {
    /// Copies what the check needs out of the state of WinVerifyTrust,
    /// which is valid until the state closes.
    unsafe fn from_state(state: HANDLE) -> Result<Self, String> {
        let provider = WTHelperProvDataFromStateData(state);
        if provider.is_null() {
            return Err("Windows could not read its signature".into());
        }
        signature_fits(&*provider)?;
        let signer = WTHelperGetProvSignerFromChain(provider, 0, 0, 0);
        if signer.is_null() {
            return Err("its signature names no signer".into());
        }
        if (*signer).dwError != 0 {
            return Err(format!(
                "its signature does not fit the file (signer error 0x{:08X})",
                (*signer).dwError
            ));
        }
        let mut time_servers = Vec::new();
        for index in 0..(*signer).csCounterSigners {
            let time_server = WTHelperGetProvSignerFromChain(provider, 0, 1, index);
            if !time_server.is_null() && (*time_server).dwError != 0 {
                return Err(format!(
                    "its timestamp is not valid (error 0x{:08X})",
                    (*time_server).dwError
                ));
            }
            time_servers.push(first_certificate(
                time_server,
                "its timestamp names no signer",
            )?);
        }
        Ok(Self {
            signer: first_certificate(signer, "its signature names no signer")?,
            time_servers,
            made: (*signer).sftVerifyAsOf,
        })
    }
}

/// The certificate of a signer or of a time server.
unsafe fn first_certificate(
    signer: *mut ProviderSigner,
    missing: &str,
) -> Result<Certificate, String> {
    if signer.is_null() {
        return Err(missing.into());
    }
    let certificate = WTHelperGetProvCertFromChain(signer, 0);
    if certificate.is_null() || (*certificate).pCert.is_null() {
        return Err(missing.into());
    }
    Ok(Certificate::duplicate((*certificate).pCert))
}

/// Fails unless every step up to the check of the signature passed.
unsafe fn signature_fits(provider: &ProviderData) -> Result<(), String> {
    let steps = items(provider.padwTrustStepErrors, provider.cdwTrustStepErrors);
    if steps.len() <= LAST_SIGNATURE_STEP {
        return Err("Windows did not check its signature".into());
    }
    match steps[..=LAST_SIGNATURE_STEP]
        .iter()
        .enumerate()
        .find(|(_, error)| **error != 0)
    {
        Some((step, error)) => Err(format!(
            "its signature does not fit the file (step {step}, error 0x{error:08X})"
        )),
        None => Ok(()),
    }
}

/// The `count` items that start at `first`. Windows gives a null pointer
/// for a list without items, which a slice must not start at.
unsafe fn items<'a, T>(first: *const T, count: DWORD) -> &'a [T] {
    if first.is_null() || count == 0 {
        &[]
    } else {
        slice::from_raw_parts(first, count as usize)
    }
}

/// `what`, with the last error of Windows.
fn windows_error(what: &str) -> String {
    format!("{what}: {}", io::Error::last_os_error())
}

#[cfg(test)]
mod tests {
    use super::{check, Publisher};
    use std::path::PathBuf;

    /// A file that tests/fixtures/authenticode/make.sh made.
    fn file(name: &str) -> PathBuf {
        [
            env!("CARGO_MANIFEST_DIR"),
            "tests",
            "fixtures",
            "authenticode",
            name,
        ]
        .iter()
        .collect()
    }

    /// The reason the service that `service` stands for refuses `package`.
    fn refusal(service: &str, package: &str) -> String {
        check(&file(service), &file(package)).expect_err(package)
    }

    #[test]
    fn a_later_certificate_of_the_publisher_is_accepted() {
        // The two certificates were valid for three days each, five months
        // apart, and both have expired.
        assert_eq!(
            check(&file("service.exe"), &file("same-publisher.exe")),
            Ok(Publisher::Same)
        );
        // The Azure identity names the publisher, whatever the subject says.
        assert_eq!(
            check(&file("service.exe"), &file("same-identity-other-name.exe")),
            Ok(Publisher::Same)
        );
        // A service whose signature has no timestamp still names its
        // publisher.
        assert_eq!(
            check(
                &file("service-no-timestamp.exe"),
                &file("same-publisher.exe")
            ),
            Ok(Publisher::Same)
        );
    }

    #[test]
    fn a_subject_names_the_publisher_of_a_certificate_without_an_azure_identity() {
        assert_eq!(
            check(&file("service-subject.exe"), &file("same-subject.exe")),
            Ok(Publisher::Same)
        );
        let reason = refusal("service-subject.exe", "other-subject.exe");
        assert!(reason.contains("another publisher"), "{reason}");
        let reason = refusal("service-subject.exe", "same-publisher.exe");
        assert!(reason.contains("another publisher"), "{reason}");
    }

    #[test]
    fn a_certificate_without_key_usages_is_compared_by_its_subject() {
        assert_eq!(
            check(&file("service-no-usage.exe"), &file("same-no-usage.exe")),
            Ok(Publisher::Same)
        );
        let reason = refusal("service-no-usage.exe", "same-subject.exe");
        assert!(reason.contains("another publisher"), "{reason}");
    }

    #[test]
    fn another_publisher_under_the_same_root_is_refused() {
        let reason = refusal("service.exe", "other-identity-same-name.exe");
        assert!(reason.contains("another publisher"), "{reason}");
    }

    #[test]
    fn the_same_names_under_another_root_are_refused() {
        let reason = refusal("service.exe", "other-root.exe");
        assert!(reason.contains("does not lead to the root"), "{reason}");
    }

    #[test]
    fn a_file_without_a_signature_that_fits_it_is_refused() {
        let reason = refusal("service.exe", "unsigned.exe");
        assert!(reason.contains("no signature"), "{reason}");
        let reason = refusal("service.exe", "changed-after-signing.exe");
        assert!(reason.contains("does not fit the file"), "{reason}");
    }

    #[test]
    fn a_signature_that_another_key_made_is_refused() {
        // The file holds a certificate of the publisher, and the key of
        // another certificate with the same name and serial number made
        // the signature.
        let reason = refusal("service.exe", "signature-by-another-key.exe");
        assert!(reason.contains("does not fit the file"), "{reason}");
    }

    #[test]
    fn a_certificate_that_another_publisher_issued_is_refused() {
        // The certificate has the identity of the publisher, and its issuer
        // is the signing certificate of another publisher under the root.
        let reason = refusal("service.exe", "certificate-from-another-publisher.exe");
        assert!(reason.contains("does not lead to the root"), "{reason}");
    }

    #[test]
    fn the_first_signature_of_a_package_names_its_publisher() {
        assert_eq!(
            check(
                &file("service.exe"),
                &file("second-signature-by-another-publisher.exe")
            ),
            Ok(Publisher::Same)
        );
        let reason = refusal("service.exe", "second-signature-by-the-publisher.exe");
        assert!(reason.contains("another publisher"), "{reason}");
    }

    #[test]
    fn a_package_needs_no_root_certificate_of_its_own() {
        // The root comes from the service, so a signature of the publisher
        // without it in the file still leads to it.
        assert_eq!(
            check(&file("service.exe"), &file("service-no-root.exe")),
            Ok(Publisher::Same)
        );
    }

    #[test]
    fn a_file_that_is_no_program_is_refused() {
        let reason = refusal("service.exe", "make.sh");
        assert!(reason.contains("no program"), "{reason}");
        let reason = refusal("service.exe", "no-such-file.exe");
        assert!(reason.contains("could not be opened"), "{reason}");
    }

    #[test]
    fn an_expired_certificate_needs_a_timestamp_from_its_validity() {
        for package in [
            "no-timestamp.exe",
            "claimed-time.exe",
            "timestamp-after-expiry.exe",
        ] {
            let reason = refusal("service.exe", package);
            assert!(
                reason.contains("not valid at the time"),
                "{package}: {reason}"
            );
        }
    }

    #[test]
    fn a_timestamp_counts_only_from_a_time_server_under_the_same_root() {
        let reason = refusal("service.exe", "timestamp-from-other-root.exe");
        assert!(reason.contains("its timestamp does not lead"), "{reason}");
        // Windows does not read a timestamp whose signer is no time server,
        // so the certificate counts as expired.
        let reason = refusal("service.exe", "timestamp-by-code-signer.exe");
        assert!(reason.contains("not valid at the time"), "{reason}");
    }

    #[test]
    fn a_program_without_a_signature_checks_no_package() {
        assert_eq!(
            check(&file("unsigned.exe"), &file("other-root.exe")),
            Ok(Publisher::Unknown)
        );
    }

    #[test]
    fn a_program_whose_signature_cannot_be_read_refuses_every_package() {
        // The program has a signature block, so it is not a build without
        // a signature, and it cannot name its publisher.
        for package in ["same-publisher.exe", "other-root.exe", "unsigned.exe"] {
            let reason = refusal("service-unreadable-signature.exe", package);
            assert!(reason.contains("could not be read"), "{package}: {reason}");
        }
    }

    #[test]
    fn a_signed_program_without_its_root_refuses_every_package() {
        let reason = refusal("service-no-root.exe", "same-publisher.exe");
        assert!(reason.contains("holds no root certificate"), "{reason}");
    }
}
